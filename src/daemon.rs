//! # Real-Time Linux `inotify` Watcher Daemon
//!
//! ## Overview
//! Watches `~/.gemini/antigravity-cli/{conversations,brain,annotations}` using the Linux kernel
//! `inotify` subsystem. Performs immediate forward hardlinking when new `.db`, `.db-wal`, `.db-shm`,
//! or `brain/<uuid>` files appear, and triggers `< 1ms` reverse restoration when the Antigravity
//! IDE or CLI accesses `annotations/<uuid>.pbtxt` for a garbage-collected conversation.
//!
//! ## Search Tags
//! #inotify, #daemon, #auto-restore, #linux-syscalls, #real-time

use crate::restore::restore_session;
use crate::shim::{PatchOutcome, ensure_agy_binary_unbounded, restore_all_from_vault};
use crate::vault_engine::{VaultConfig, is_uuid_like, sync_all, sync_session};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const IN_ACCESS: u32 = 0x0000_0001;
const IN_MODIFY: u32 = 0x0000_0002;
const IN_ATTRIB: u32 = 0x0000_0004;
const IN_CLOSE_WRITE: u32 = 0x0000_0008;
const IN_OPEN: u32 = 0x0000_0020;
const IN_MOVED_TO: u32 = 0x0000_0080;
const IN_CREATE: u32 = 0x0000_0100;
const IN_DELETE: u32 = 0x0000_0200;

const IN_CLOEXEC: i32 = 0o2000000;
const IN_NONBLOCK: i32 = 0o0004000;
const POLLIN: i16 = 0x0001;

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

#[repr(C)]
struct InotifyEventHeader {
    wd: i32,
    mask: u32,
    cookie: u32,
    len: u32,
}

unsafe extern "C" {
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, pathname: *const i8, mask: u32) -> i32;
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn close(fd: i32) -> i32;
}

/// Configuration for the real-time `inotify` watcher daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// Underlying active and vault directory configuration.
    pub vault: VaultConfig,
    /// Poll interval in milliseconds for `inotify` + background reconciliation.
    pub poll_ms: i32,
    /// Optional maximum number of poll ticks to execute before exiting (`None` = run forever).
    pub max_ticks: Option<usize>,
}

/// Runtime telemetry counters collected by [`run_daemon`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonStats {
    /// Number of poll loop iterations completed.
    pub ticks_completed: usize,
    /// Number of raw `inotify` kernel events processed.
    pub inotify_events_processed: usize,
    /// Number of real-time forward session syncs triggered by `inotify`.
    pub forward_syncs_triggered: usize,
    /// Number of `< 1ms` reverse session restorations triggered by `inotify`.
    pub auto_restores_triggered: usize,
    /// Number of times `bin/agy` was patched to disable the 500-conversation GC limit.
    pub binary_patches_applied: usize,
}

/// RAII wrapper around a Linux `inotify` file descriptor and its directory watch descriptors.
struct InotifyGuard {
    fd: i32,
    conversations_wd: i32,
    brain_wd: i32,
    annotations_wd: i32,
    bin_wd: i32,
}

impl InotifyGuard {
    fn new(config: &VaultConfig) -> Result<Self, String> {
        let bin_dir = config.agy_dir.join("bin");
        fs::create_dir_all(config.active_conversations_dir()).map_err(|e| e.to_string())?;
        fs::create_dir_all(config.active_brain_dir()).map_err(|e| e.to_string())?;
        fs::create_dir_all(config.active_annotations_dir()).map_err(|e| e.to_string())?;
        fs::create_dir_all(&bin_dir).map_err(|e| e.to_string())?;

        // SAFETY: `inotify_init1` is a standard thread-safe Linux syscall taking bitflags.
        let fd = unsafe { inotify_init1(IN_NONBLOCK | IN_CLOEXEC) };
        if fd < 0 {
            return Err("inotify_init1 syscall failed".to_string());
        }

        let conv_mask = IN_CREATE | IN_MODIFY | IN_CLOSE_WRITE | IN_MOVED_TO | IN_ATTRIB | IN_DELETE;
        let brain_mask = IN_CREATE | IN_MOVED_TO | IN_MODIFY | IN_DELETE;
        let ann_mask = IN_ACCESS | IN_OPEN | IN_MODIFY | IN_CREATE | IN_MOVED_TO;
        let bin_mask = IN_CREATE | IN_MOVED_TO | IN_CLOSE_WRITE;

        let conversations_wd = Self::add_watch(fd, &config.active_conversations_dir(), conv_mask)?;
        let brain_wd = Self::add_watch(fd, &config.active_brain_dir(), brain_mask)?;
        let annotations_wd = Self::add_watch(fd, &config.active_annotations_dir(), ann_mask)?;
        let bin_wd = Self::add_watch(fd, &bin_dir, bin_mask)?;

        Ok(Self {
            fd,
            conversations_wd,
            brain_wd,
            annotations_wd,
            bin_wd,
        })
    }

    fn add_watch(fd: i32, path: &Path, mask: u32) -> Result<i32, String> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("invalid NUL byte in watch path {}", path.display()))?;
        // SAFETY: `c_path` is a valid NUL-terminated C string and `fd` is a valid inotify descriptor.
        let wd = unsafe { inotify_add_watch(fd, c_path.as_ptr(), mask) };
        if wd < 0 {
            return Err(format!("inotify_add_watch failed for {}", path.display()));
        }
        Ok(wd)
    }
}

impl Drop for InotifyGuard {
    fn drop(&mut self) {
        if self.fd >= 0 {
            // SAFETY: `self.fd` is owned exclusively by `InotifyGuard`.
            unsafe {
                let _ = close(self.fd);
            }
        }
    }
}

/// Extracts a canonical UUID from a conversation database filename (`<uuid>.db`, `.db-wal`, `.db-shm`).
fn extract_conversation_uuid(file_name: &str) -> Option<&str> {
    let stem = file_name
        .strip_suffix(".db")
        .or_else(|| file_name.strip_suffix(".db-wal"))
        .or_else(|| file_name.strip_suffix(".db-shm"))?;
    if is_uuid_like(stem) {
        Some(stem)
    } else {
        None
    }
}

/// Handles an `inotify` event inside `<agy_dir>/annotations`.
fn handle_annotation_event(config: &VaultConfig, mask: u32, name: &str, stats: &mut DaemonStats) {
    let Some(uuid) = name.strip_suffix(".pbtxt").filter(|u| is_uuid_like(u)) else {
        return;
    };
    if (mask & (IN_ACCESS | IN_OPEN | IN_MODIFY)) != 0 {
        let a_db = config.active_conversations_dir().join(format!("{uuid}.db"));
        let v_db = config.vault_conversations_dir().join(format!("{uuid}.db"));
        let v_br = config.vault_brain_dir().join(uuid);
        if !a_db.is_file()
            && (v_db.is_file() || v_br.is_dir())
            && let Ok(rep) = restore_session(config, uuid)
        {
            stats.auto_restores_triggered += 1;
            println!(
                "[agy-vault] auto-restored session {} in {} us (db={}, brain_files={})",
                rep.uuid, rep.elapsed_micros, rep.db_restored, rep.brain_files_restored
            );
        }
    }
    if (mask & (IN_CREATE | IN_MOVED_TO | IN_MODIFY)) != 0 {
        let _ = sync_session(config, uuid);
    }
}

/// Processes a single `inotify` event from `conversations/`, `brain/`, `annotations/`, or `bin/`.
fn handle_inotify_event(
    config: &VaultConfig,
    guard: &InotifyGuard,
    hdr: &InotifyEventHeader,
    name: &str,
    stats: &mut DaemonStats,
) {
    if name.is_empty() || name.starts_with('.') {
        return;
    }
    if hdr.wd == guard.bin_wd && name == "agy" {
        let bin_path = config.agy_dir.join("bin/agy");
        if let Ok(PatchOutcome::NewlyPatched(off)) = ensure_agy_binary_unbounded(&bin_path) {
            stats.binary_patches_applied += 1;
            let _ = restore_all_from_vault(config);
            println!(
                "[agy-vault] auto-patched updated agy binary at offset 0x{off:x} (MaxConversations: 500 -> 0)"
            );
        }
    } else if hdr.wd == guard.annotations_wd {
        handle_annotation_event(config, hdr.mask, name, stats);
    } else if hdr.wd == guard.conversations_wd {
        if let Some(uuid) = extract_conversation_uuid(name) {
            if (hdr.mask & IN_DELETE) != 0 {
                if restore_session(config, uuid).is_ok() {
                    stats.auto_restores_triggered += 1;
                }
            } else if sync_session(config, uuid).is_ok() {
                stats.forward_syncs_triggered += 1;
            }
        }
    } else if hdr.wd == guard.brain_wd && is_uuid_like(name) {
        if (hdr.mask & IN_DELETE) != 0 {
            if restore_session(config, name).is_ok() {
                stats.auto_restores_triggered += 1;
            }
        } else if sync_session(config, name).is_ok() {
            stats.forward_syncs_triggered += 1;
        }
    }
}

/// Drains all pending events from the non-blocking `inotify` descriptor.
fn drain_inotify_events(config: &VaultConfig, guard: &InotifyGuard, stats: &mut DaemonStats) {
    let mut buf = [0u8; 16384];
    let hdr_sz = std::mem::size_of::<InotifyEventHeader>();

    loop {
        // SAFETY: `buf` is a stack array of 16384 bytes and `guard.fd` is non-blocking.
        let n = unsafe { read(guard.fd, buf.as_mut_ptr(), buf.len()) };
        if n <= 0 {
            break;
        }
        let (bytes_read, mut off) = (n as usize, 0usize);
        while off + hdr_sz <= bytes_read {
            // SAFETY: We copy the unaligned header bytes into a stack-aligned struct.
            let hdr: InotifyEventHeader = unsafe {
                std::ptr::read_unaligned(buf.as_ptr().add(off) as *const InotifyEventHeader)
            };
            off += hdr_sz;
            let nlen = hdr.len as usize;
            if off + nlen > bytes_read {
                break;
            }
            let slice = &buf[off..off + nlen];
            let end = slice.iter().position(|&b| b == 0).unwrap_or(nlen);
            if let Ok(s) = std::str::from_utf8(&slice[..end]) {
                stats.inotify_events_processed += 1;
                handle_inotify_event(config, guard, &hdr, s, stats);
            }
            off += nlen;
        }
    }
}

/// Runs the real-time `inotify` watcher daemon and automatic `bin/agy` signature patcher.
pub fn run_daemon(daemon_cfg: &DaemonConfig) -> Result<DaemonStats, String> {
    let mut stats = DaemonStats::default();
    let bin_path = daemon_cfg.vault.agy_dir.join("bin/agy");
    if let Ok(PatchOutcome::NewlyPatched(off)) = ensure_agy_binary_unbounded(&bin_path) {
        stats.binary_patches_applied += 1;
        println!(
            "[agy-vault] patched agy binary at offset 0x{off:x} (MaxConversations: 500 -> 0)"
        );
    }
    let _ = restore_all_from_vault(&daemon_cfg.vault);
    let initial = sync_all(&daemon_cfg.vault)?;
    println!(
        "[agy-vault] initial sync complete: active_db={}, brain_dirs={}, annotations={}, new_links={}, elapsed={}ms",
        initial.active_db_synced,
        initial.brain_dirs_synced,
        initial.annotations_synced,
        initial.hardlinks_created,
        initial.elapsed_ms
    );

    let guard = InotifyGuard::new(&daemon_cfg.vault)?;

    loop {
        let mut pfd = PollFd {
            fd: guard.fd,
            events: POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a valid single-element `PollFd` struct on the stack.
        let ready = unsafe { poll(&mut pfd as *mut PollFd, 1, daemon_cfg.poll_ms) };
        if ready > 0 && (pfd.revents & POLLIN) != 0 {
            drain_inotify_events(&daemon_cfg.vault, &guard, &mut stats);
        }

        stats.ticks_completed += 1;
        if stats.ticks_completed.is_multiple_of(300) {
            let _ = sync_all(&daemon_cfg.vault);
        }

        if let Some(max) = daemon_cfg.max_ticks
            && stats.ticks_completed >= max
        {
            drain_inotify_events(&daemon_cfg.vault, &guard, &mut stats);
            let _ = sync_all(&daemon_cfg.vault);
            break;
        }
    }

    Ok(stats)
}
