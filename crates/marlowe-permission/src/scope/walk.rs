//! The handle walk. **This is the wall.**
//!
//! ADR-002 removed the kernel backstop, so a path check defeated by string manipulation is the
//! whole protection gone. `super::request` refuses ambiguous *spellings*; this module is what
//! makes containment true regardless of what happens on the filesystem while it runs.
//!
//! # There is no check-then-open here, on either platform
//!
//! The shape that fails is: resolve the string, decide it is in scope, then open it. Between
//! the decision and the open, a component can be replaced by a link that points elsewhere — the
//! check was correct and the open still landed outside. Both implementations below avoid it by
//! never resolving a string the kernel will resolve again.
//!
//! **POSIX.** `openat` from the parent directory's descriptor, one component at a time, with
//! `O_NOFOLLOW`. A symlink in any position fails the open with `ELOOP` rather than being
//! followed. There is no string for anyone to swap: each step names a single component relative
//! to a descriptor we already hold.
//!
//! **Windows.** There is no `openat` in Win32, so containment comes from **pinning** instead:
//! every directory on the path is opened with a share mode that excludes `FILE_SHARE_DELETE`,
//! and the handles are held for the whole walk. A directory cannot be renamed or deleted while
//! such a handle is open, so the prefix cannot be swapped out underneath the next open. Each
//! component is additionally opened with `FILE_FLAG_OPEN_REPARSE_POINT` and refused if it
//! carries `FILE_ATTRIBUTE_REPARSE_POINT`, which is how junctions and symlinks are caught rather
//! than traversed.
//!
//! The root's identity is read before and after the walk and compared. With pinning in place
//! that comparison should never fail — which is the point: it is a check on whether the pinning
//! worked, not a substitute for it.
//!
//! # The observer seam, and why it is in production code
//!
//! [`WalkObserver`] is called at the exact moment a race would have to land — after component
//! *k* is open and before component *k+1* is. In production it is `()`, a zero-sized no-op that
//! compiles away. The TOCTOU test uses it to plant a reparse point *deterministically* at the
//! vulnerable instant, instead of racing a thread and hoping.
//!
//! That matters for what the suite proves. `tests/toctou.rs` runs the same interleaving against
//! a deliberately naive check-then-open implementation and **asserts that it escapes**. Without
//! that half, a passing suite would be evidence that the test never raced — which is exactly the
//! believed-boundary failure brief §8.3 warns about.

use std::fs::File;
use std::path::{Path, PathBuf};

use super::request::RequestedPath;
use super::{Access, ScopeError};

/// Called between components during a walk. Production is `()`.
pub trait WalkObserver {
    /// `index` components have been opened and verified; the next open is about to happen.
    fn after_component(&self, _index: usize, _opened: &Path) {}
}

impl WalkObserver for () {}

pub struct Opened {
    pub handle: File,
    pub resolved: PathBuf,
}

/// Open `request` beneath `root`, refusing anything that leaves it.
///
/// `access` applies **only to the final component**. Every directory on the way is opened
/// read-only and refused if it is a reparse point, whatever the caller intends to do at the end
/// — a write request is not a licence to traverse differently.
///
/// [`Access::CreateOrOpen`] creates the final component **inside the already-verified parent**,
/// which is why creation is safe: the parent was walked under the same discipline and is still
/// held open (pinned on Windows, an `openat` descriptor on POSIX), so the prefix cannot have
/// been swapped between the walk and the create.
pub fn open_within(
    root: &Path,
    request: &RequestedPath,
    access: Access,
    observer: &dyn WalkObserver,
) -> Result<Opened, ScopeError> {
    imp::open_within(root, request, access, observer)
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    // Win32 constants. Spelled here rather than pulled from a bindings crate: these five values
    // have been stable since Windows NT and a dependency for them would be a supply-chain edge
    // on the one component with no kernel backstop behind it.
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    // FILE_SHARE_DELETE (0x4) is deliberately ABSENT. Its absence is the pinning.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;

    fn open_component(
        path: &Path,
        follow_reparse: bool,
        access: Access,
    ) -> std::io::Result<File> {
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
        if !follow_reparse {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        let mut o = OpenOptions::new();
        o.read(true);
        match access {
            Access::Read => {}
            Access::ReadWrite => {
                o.write(true);
            }
            Access::CreateOrOpen => {
                o.write(true).create(true);
            }
        }
        o
            // No FILE_SHARE_DELETE: while this handle is open the object cannot be renamed or
            // deleted, so the prefix we have walked cannot be swapped underneath us.
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(flags)
            .open(path)
    }

    /// Real object identity: volume serial number plus file index, read from the **handle**.
    ///
    /// Via `same-file`, because `MetadataExt::file_index` and `volume_serial_number` are still
    /// unstable in std. A weaker stand-in — creation time and size — would answer a question
    /// adjacent to the one being asked and would read the same whether or not the object had
    /// been replaced, which is the proxy failure this project has logged twelve times.
    fn identity(path: &Path) -> std::io::Result<same_file::Handle> {
        same_file::Handle::from_path(path)
    }

    pub fn open_within(
        root: &Path,
        request: &RequestedPath,
        access: Access,
        observer: &dyn WalkObserver,
    ) -> Result<Opened, ScopeError> {
        let requested = request.as_relative();

        // The root itself may legitimately be reached through a link the operator set up, so it
        // is opened following reparse points. Everything BELOW it is not.
        let root_handle = open_component(root, true, Access::Read).map_err(|e| ScopeError::Unopenable {
            requested: root.display().to_string(),
            detail: e.to_string(),
        })?;
        let root_identity = identity(root).map_err(|e| ScopeError::Unopenable {
            requested: root.display().to_string(),
            detail: e.to_string(),
        })?;

        // Every prefix handle stays alive until the walk finishes. Dropping one early would
        // unpin that directory and reopen the window this function exists to close.
        let mut pinned: Vec<File> = vec![root_handle];
        let mut accumulated = root.to_path_buf();

        let components = request.components();
        for (i, component) in components.iter().enumerate() {
            accumulated.push(component);
            let is_last = i + 1 == components.len();
            // Directories on the way are always read-only. A write request is not a licence to
            // traverse differently.
            let component_access = if is_last { access } else { Access::Read };
            let handle = open_component(&accumulated, false, component_access).map_err(|e| {
                ScopeError::Unopenable {
                    requested: requested.clone(),
                    detail: format!("{}: {e}", accumulated.display()),
                }
            })?;
            let meta = handle.metadata().map_err(|e| ScopeError::Unopenable {
                requested: requested.clone(),
                detail: e.to_string(),
            })?;

            if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                // A junction, a directory symlink or a file symlink. Refused, not followed —
                // this is the whole Windows half of the traversal class.
                return Err(ScopeError::OutsideScope { requested: requested.clone() });
            }

            if !is_last && meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY == 0 {
                return Err(ScopeError::Unopenable {
                    requested: requested.clone(),
                    detail: format!("{} is not a directory", accumulated.display()),
                });
            }

            pinned.push(handle);
            observer.after_component(i, &accumulated);
        }

        let final_handle = pinned
            .pop()
            .unwrap_or_else(|| unreachable!("the root handle is always pushed before the loop"));

        // Final-handle identity verification. The root path still names the object the walk
        // started from — if something repointed it mid-walk, the volume/index pair differs and
        // the result is refused rather than returned.
        //
        // With pinning in place this should never fire. That is deliberate: it is a check on
        // whether the pinning worked, and a boundary whose only evidence is "we believe the
        // share mode does what we think" is the kind this project stopped accepting.
        let after = identity(root).map_err(|e| ScopeError::Unopenable {
            requested: requested.clone(),
            detail: e.to_string(),
        })?;
        if after != root_identity {
            return Err(ScopeError::IdentityChanged { requested });
        }
        // `pinned` drops here, releasing the prefix. Nothing below re-resolves a string, so the
        // release is safe: the caller holds an open handle to the object that was verified.
        drop(pinned);

        Ok(Opened { handle: final_handle, resolved: accumulated })
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use rustix::fs::{Mode, OFlags};

    pub fn open_within(
        root: &Path,
        request: &RequestedPath,
        access: Access,
        observer: &dyn WalkObserver,
    ) -> Result<Opened, ScopeError> {
        let requested = request.as_relative();

        // The root may legitimately be a symlink the operator set up (`/tmp` on macOS is one),
        // so it is opened following links. Everything below it is not.
        let mut dir = rustix::fs::open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| ScopeError::Unopenable {
            requested: root.display().to_string(),
            detail: e.to_string(),
        })?;

        let mut accumulated = root.to_path_buf();
        let components = request.components();

        for (i, component) in components.iter().enumerate() {
            accumulated.push(component);
            let is_last = i + 1 == components.len();
            // O_NOFOLLOW is the containment: a symlink in this position fails the open with
            // ELOOP rather than being traversed. openat means there is no string for anyone to
            // swap — the component is named relative to a descriptor already held.
            let mut flags = OFlags::NOFOLLOW | OFlags::CLOEXEC;
            if !is_last {
                flags |= OFlags::DIRECTORY | OFlags::RDONLY;
            } else {
                match access {
                    Access::Read => flags |= OFlags::RDONLY,
                    Access::ReadWrite => flags |= OFlags::RDWR,
                    // O_CREAT with O_NOFOLLOW: if the final component is a symlink the open
                    // fails with ELOOP rather than creating through it.
                    Access::CreateOrOpen => flags |= OFlags::RDWR | OFlags::CREATE,
                }
            }
            let mode = if matches!(access, Access::CreateOrOpen) {
                Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH
            } else {
                Mode::empty()
            };
            let opened = rustix::fs::openat(&dir, component.as_str(), flags, mode)
                .map_err(|e| {
                    if e == rustix::io::Errno::LOOP || e == rustix::io::Errno::MLINK {
                        ScopeError::OutsideScope { requested: requested.clone() }
                    } else {
                        ScopeError::Unopenable {
                            requested: requested.clone(),
                            detail: format!("{}: {e}", accumulated.display()),
                        }
                    }
                })?;
            dir = opened;
            observer.after_component(i, &accumulated);
        }

        Ok(Opened { handle: File::from(dir), resolved: accumulated })
    }
}
