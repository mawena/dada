//! `winfsp::filesystem::FileSystemContext` on top of a dada `Volume`.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use libdada::{Attr, DadaError, FileDevice, FileKind, Ino, SetAttr, Volume};
use log::{error, warn};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, U16CStr};

use crate::convert::*;
use crate::names;

/// CreateOptions flag: the object to create is a directory.
const FILE_DIRECTORY_FILE: u32 = 0x1;
/// Cleanup flag: delete the file.
const CLEANUP_DELETE: u32 = 0x1;

pub type SharedVolume = Arc<Mutex<Option<Volume<FileDevice>>>>;

/// An open file or directory.
pub struct DadaFile {
    ino: Ino,
    dir_buffer: DirBuffer,
}

pub struct DadaWinFs {
    volume: SharedVolume,
    /// Self-relative security descriptor given to every file.
    security: Vec<u8>,
    label: String,
}

fn status(code: u32) -> FspError {
    FspError::NTSTATUS(code as i32)
}

fn fsp(e: DadaError) -> FspError {
    if matches!(e, DadaError::Corrupt(_) | DadaError::Io(_)) {
        error!("{e}");
    }
    status(ntstatus(&e))
}

fn path_of(name: &U16CStr) -> Result<Vec<String>, FspError> {
    let path = name
        .to_string()
        .map_err(|_| status(STATUS_OBJECT_NAME_INVALID))?;
    Ok(names::components(&path))
}

/// Walks `comps` from the root. A missing intermediate directory is
/// reported as a missing path rather than a missing name.
fn resolve(vol: &mut Volume<FileDevice>, comps: &[String]) -> Result<Attr, FspError> {
    let mut attr = vol.getattr(vol.root()).map_err(fsp)?;
    for (i, name) in comps.iter().enumerate() {
        attr = vol.lookup(attr.ino, name).map_err(|e| match e {
            DadaError::NotFound | DadaError::NotDir if i + 1 < comps.len() => {
                status(STATUS_OBJECT_PATH_NOT_FOUND)
            }
            e => fsp(e),
        })?;
    }
    Ok(attr)
}

/// Parent directory and last name of `comps`.
fn resolve_parent<'c>(
    vol: &mut Volume<FileDevice>,
    comps: &'c [String],
) -> Result<(Ino, &'c str), FspError> {
    let (name, parents) = comps
        .split_last()
        .ok_or(status(STATUS_OBJECT_NAME_INVALID))?;
    let parent = resolve(vol, parents).map_err(|_| status(STATUS_OBJECT_PATH_NOT_FOUND))?;
    if parent.kind != FileKind::Directory {
        return Err(status(STATUS_OBJECT_PATH_NOT_FOUND));
    }
    Ok((parent.ino, name))
}

fn fill_info(info: &mut FileInfo, attr: &Attr, block_size: u64) {
    info.file_attributes = windows_attributes(attr.kind, attr.mode, attr.win_attrs);
    info.reparse_tag = if attr.kind == FileKind::Symlink {
        IO_REPARSE_TAG_SYMLINK
    } else {
        0
    };
    info.file_size = attr.size;
    info.allocation_size = (attr.blocks * 512).max(attr.size.div_ceil(block_size) * block_size);
    info.creation_time = to_filetime(attr.btime);
    info.last_access_time = to_filetime(attr.atime);
    info.last_write_time = to_filetime(attr.mtime);
    info.change_time = to_filetime(attr.ctime);
    info.index_number = attr.ino;
    info.hard_links = 0;
    info.ea_size = 0;
}

impl DadaWinFs {
    pub fn new(volume: SharedVolume, security: Vec<u8>, label: String) -> Self {
        DadaWinFs {
            volume,
            security,
            label,
        }
    }

    fn with<T>(
        &self,
        op: impl FnOnce(&mut Volume<FileDevice>) -> Result<T, FspError>,
    ) -> Result<T, FspError> {
        let mut guard = self.volume.lock().unwrap_or_else(|p| p.into_inner());
        let vol = guard.as_mut().ok_or(status(STATUS_UNEXPECTED_IO_ERROR))?;
        op(vol)
    }

    fn block_size(vol: &Volume<FileDevice>) -> u64 {
        u64::from(vol.statfs().block_size)
    }

    fn copy_security(&self, buffer: Option<&mut [c_void]>) {
        if let Some(buffer) = buffer {
            if buffer.len() >= self.security.len() {
                // SAFETY: `buffer` holds at least `self.security.len()` bytes
                // (checked above) and does not overlap our own vector.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.security.as_ptr(),
                        buffer.as_mut_ptr().cast::<u8>(),
                        self.security.len(),
                    );
                }
            }
        }
    }

    /// Applies Windows attributes to an inode.
    fn set_attributes(
        vol: &mut Volume<FileDevice>,
        ino: Ino,
        attrs: u32,
    ) -> Result<Attr, FspError> {
        let attr = vol.getattr(ino).map_err(fsp)?;
        let (mode, win_attrs) = apply_windows_attributes(attrs, attr.mode);
        let changes = SetAttr {
            mode: Some(mode),
            win_attrs: Some(win_attrs),
            ..SetAttr::default()
        };
        vol.setattr(ino, &changes).map_err(fsp)
    }

    fn reparse_buffer(
        vol: &mut Volume<FileDevice>,
        attr: &Attr,
        buffer: &mut [u8],
    ) -> Result<u64, FspError> {
        if attr.kind != FileKind::Symlink {
            return Err(status(STATUS_NOT_A_REPARSE_POINT));
        }
        let target = vol.readlink(attr.ino).map_err(fsp)?;
        let data = symlink_reparse_buffer(&target);
        let dst = buffer
            .get_mut(..data.len())
            .ok_or(status(STATUS_BUFFER_TOO_SMALL))?;
        dst.copy_from_slice(&data);
        Ok(data.len() as u64)
    }
}

impl FileSystemContext for DadaWinFs {
    type FileContext = DadaFile;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        if let Some(security) = reparse_point_resolver(file_name) {
            return Ok(security);
        }
        let comps = path_of(file_name)?;
        let attr = self.with(|vol| resolve(vol, &comps))?;
        self.copy_security(security_descriptor);
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: self.security.len() as u64,
            attributes: windows_attributes(attr.kind, attr.mode, attr.win_attrs),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let comps = path_of(file_name)?;
        self.with(|vol| {
            let attr = resolve(vol, &comps)?;
            fill_info(file_info.as_mut(), &attr, Self::block_size(vol));
            Ok(DadaFile {
                ino: attr.ino,
                dir_buffer: DirBuffer::new(),
            })
        })
    }

    fn close(&self, _context: Self::FileContext) {}

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let raw = file_name
            .to_string()
            .map_err(|_| status(STATUS_OBJECT_NAME_INVALID))?;
        let last = raw.rsplit('\\').next().unwrap_or_default();
        if names::is_reserved(last) {
            return Err(status(STATUS_OBJECT_NAME_INVALID));
        }
        let comps = names::components(&raw);
        self.with(|vol| {
            let (parent, name) = resolve_parent(vol, &comps)?;
            let attr = if create_options & FILE_DIRECTORY_FILE != 0 {
                vol.mkdir(parent, name, 0o755, 0, 0)
            } else {
                vol.create(parent, name, 0o644, 0, 0)
            }
            .map_err(fsp)?;
            let extra = file_attributes
                & (FILE_ATTRIBUTE_READONLY
                    | FILE_ATTRIBUTE_HIDDEN
                    | FILE_ATTRIBUTE_SYSTEM
                    | FILE_ATTRIBUTE_ARCHIVE);
            let attr = if extra != 0 {
                Self::set_attributes(vol, attr.ino, extra)?
            } else {
                attr
            };
            fill_info(file_info.as_mut(), &attr, Self::block_size(vol));
            Ok(DadaFile {
                ino: attr.ino,
                dir_buffer: DirBuffer::new(),
            })
        })
    }

    fn cleanup(&self, _context: &Self::FileContext, file_name: Option<&U16CStr>, flags: u32) {
        if flags & CLEANUP_DELETE == 0 {
            return;
        }
        let Some(file_name) = file_name else { return };
        let result = path_of(file_name).and_then(|comps| {
            self.with(|vol| {
                let (parent, name) = resolve_parent(vol, &comps)?;
                let attr = vol.lookup(parent, name).map_err(fsp)?;
                if attr.kind == FileKind::Directory {
                    vol.rmdir(parent, name)
                } else {
                    vol.unlink(parent, name)
                }
                .map_err(fsp)
            })
        });
        if let Err(e) = result {
            warn!("delete on cleanup failed: {e}");
        }
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.with(|vol| {
            vol.sync().map_err(fsp)?;
            if let Some(context) = context {
                let attr = vol.getattr(context.ino).map_err(fsp)?;
                fill_info(file_info, &attr, Self::block_size(vol));
            }
            Ok(())
        })
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.with(|vol| {
            let attr = vol.getattr(context.ino).map_err(fsp)?;
            fill_info(file_info, &attr, Self::block_size(vol));
            Ok(())
        })
    }

    fn get_security(
        &self,
        _context: &Self::FileContext,
        security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        self.copy_security(security_descriptor);
        Ok(self.security.len() as u64)
    }

    fn set_security(
        &self,
        _context: &Self::FileContext,
        _security_information: u32,
        _modification_descriptor: winfsp::filesystem::ModificationDescriptor,
    ) -> winfsp::Result<()> {
        // dada keeps POSIX permissions only; ACL changes are accepted and ignored.
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        file_attributes: u32,
        replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.with(|vol| {
            let truncate = SetAttr {
                size: Some(0),
                ..SetAttr::default()
            };
            let attr = vol.setattr(context.ino, &truncate).map_err(fsp)?;
            let current = windows_attributes(attr.kind, attr.mode, attr.win_attrs);
            let wanted = if replace_file_attributes {
                file_attributes
            } else {
                current | file_attributes
            };
            let attr = Self::set_attributes(vol, context.ino, wanted)?;
            fill_info(file_info, &attr, Self::block_size(vol));
            Ok(())
        })
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if let Ok(lock) = context.dir_buffer.acquire(marker.is_none(), None) {
            let entries = self.with(|vol| {
                let root = vol.root();
                let bs = Self::block_size(vol);
                let mut out = Vec::new();
                for (_, entry) in vol.readdir(context.ino, 0).map_err(fsp)? {
                    // The root of a Windows volume has no `.` and `..`.
                    if context.ino == root && (entry.name == "." || entry.name == "..") {
                        continue;
                    }
                    let attr = vol.getattr(entry.ino).map_err(fsp)?;
                    let mut info: DirInfo<255> = DirInfo::new();
                    fill_info(info.file_info_mut(), &attr, bs);
                    let shown = if entry.name == "." || entry.name == ".." {
                        entry.name.clone()
                    } else {
                        names::to_windows(&entry.name)
                    };
                    info.set_name(shown)?;
                    out.push(info);
                }
                Ok(out)
            })?;
            for mut info in entries {
                lock.write(&mut info)?;
            }
        }
        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let raw_new = new_file_name
            .to_string()
            .map_err(|_| status(STATUS_OBJECT_NAME_INVALID))?;
        if names::is_reserved(raw_new.rsplit('\\').next().unwrap_or_default()) {
            return Err(status(STATUS_OBJECT_NAME_INVALID));
        }
        let from = path_of(file_name)?;
        let to = names::components(&raw_new);
        self.with(|vol| {
            let (parent, name) = resolve_parent(vol, &from)?;
            let (new_parent, new_name) = resolve_parent(vol, &to)?;
            if !replace_if_exists && vol.lookup(new_parent, new_name).is_ok() {
                // A case-only rename of the same entry is not a collision.
                let same = vol.lookup(parent, name).map(|a| a.ino).ok()
                    == vol.lookup(new_parent, new_name).map(|a| a.ino).ok();
                if !same {
                    return Err(status(STATUS_OBJECT_NAME_COLLISION));
                }
            }
            vol.rename(parent, name, new_parent, new_name).map_err(fsp)
        })
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        file_attributes: u32,
        _creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.with(|vol| {
            if file_attributes != INVALID_FILE_ATTRIBUTES {
                Self::set_attributes(vol, context.ino, file_attributes)?;
            }
            let changes = SetAttr {
                atime: (last_access_time != 0).then(|| from_filetime(last_access_time)),
                mtime: (last_write_time != 0).then(|| from_filetime(last_write_time)),
                ..SetAttr::default()
            };
            let attr = vol.setattr(context.ino, &changes).map_err(fsp)?;
            fill_info(file_info, &attr, Self::block_size(vol));
            Ok(())
        })
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        if !delete_file {
            return Ok(());
        }
        self.with(|vol| {
            let attr = vol.getattr(context.ino).map_err(fsp)?;
            if attr.kind == FileKind::Directory
                && vol.readdir(context.ino, 0).map_err(fsp)?.len() > 2
            {
                return Err(status(STATUS_DIRECTORY_NOT_EMPTY));
            }
            Ok(())
        })
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.with(|vol| {
            let current = vol.getattr(context.ino).map_err(fsp)?;
            // Allocation size changes only matter when they cut the file.
            let size = if set_allocation_size {
                (new_size < current.size).then_some(new_size)
            } else {
                Some(new_size)
            };
            let changes = SetAttr {
                size,
                ..SetAttr::default()
            };
            let attr = vol.setattr(context.ino, &changes).map_err(fsp)?;
            fill_info(file_info, &attr, Self::block_size(vol));
            Ok(())
        })
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        self.with(|vol| {
            let n = vol.read(context.ino, offset, buffer).map_err(fsp)?;
            if n == 0 && !buffer.is_empty() {
                return Err(status(STATUS_END_OF_FILE));
            }
            Ok(n as u32)
        })
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        self.with(|vol| {
            let size = vol.getattr(context.ino).map_err(fsp)?.size;
            let offset = if write_to_eof { size } else { offset };
            let data = if constrained_io {
                // Paging I/O: never extend the file.
                if offset >= size {
                    return Ok(0);
                }
                &buffer[..buffer.len().min((size - offset) as usize)]
            } else {
                buffer
            };
            let n = vol.write(context.ino, offset, data).map_err(fsp)?;
            let attr = vol.getattr(context.ino).map_err(fsp)?;
            fill_info(file_info, &attr, Self::block_size(vol));
            Ok(n as u32)
        })
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        self.with(|vol| {
            let st = vol.statfs();
            let bs = u64::from(st.block_size);
            out_volume_info.total_size = st.total_blocks * bs;
            out_volume_info.free_size = st.free_blocks * bs;
            out_volume_info.set_volume_label(&self.label);
            Ok(())
        })
    }

    fn get_reparse_point_by_name(
        &self,
        file_name: &U16CStr,
        _is_directory: bool,
        buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        let comps = path_of(file_name)?;
        self.with(|vol| {
            let attr = resolve(vol, &comps)?;
            Self::reparse_buffer(vol, &attr, buffer)
        })
    }

    fn get_reparse_point(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        self.with(|vol| {
            let attr = vol.getattr(context.ino).map_err(fsp)?;
            Self::reparse_buffer(vol, &attr, buffer)
        })
    }
}

/// Self-relative security descriptor for every file: owned by the current
/// user, full access for everyone (dada has no Windows ACLs, like a FAT key).
pub fn security_descriptor() -> Vec<u8> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{GetSecurityDescriptorLength, PSECURITY_DESCRIPTOR};

    let owner = current_user_sid().unwrap_or_else(|| "BA".to_string());
    let sddl = format!("O:{owner}G:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{owner})(A;;FA;;;WD)");
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL string outlives the call; on success the descriptor is
    // allocated by the system and freed with LocalFree below.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(sddl.as_str()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    };
    if converted.is_err() || descriptor.0.is_null() {
        warn!("cannot build the security descriptor; files will have none");
        return Vec::new();
    }
    // SAFETY: `descriptor` is a valid self-relative descriptor returned
    // above; its length is read before the bytes are copied, then it is freed
    // exactly once.
    unsafe {
        let len = GetSecurityDescriptorLength(descriptor) as usize;
        let bytes = std::slice::from_raw_parts(descriptor.0.cast::<u8>(), len).to_vec();
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        bytes
    }
}

/// String SID of the user running the process.
fn current_user_sid() -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no
    // closing; `token` receives a real handle, closed below.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    let mut len = 0u32;
    // SAFETY: a first call with no buffer only reports the size needed.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut len) };
    // u64 storage keeps TOKEN_USER properly aligned.
    let mut buffer = vec![0u64; (len as usize).div_ceil(8) + 1];
    // SAFETY: `buffer` holds at least `len` bytes, aligned for TOKEN_USER.
    let filled = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            len,
            &mut len,
        )
    };
    // SAFETY: `token` was opened above and is closed exactly once.
    let _ = unsafe { CloseHandle(token) };
    filled.ok()?;
    // SAFETY: on success the buffer starts with a TOKEN_USER whose SID
    // points inside the same buffer, which is still alive.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = PWSTR::null();
    // SAFETY: `user.User.Sid` is valid while `buffer` lives; the string is
    // allocated by the system and freed with LocalFree.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) }.ok()?;
    // SAFETY: `sid` is a valid NUL-terminated string returned above.
    let text = unsafe { sid.to_string() }.ok();
    // SAFETY: freed exactly once, after its last use.
    let _ = unsafe { LocalFree(Some(HLOCAL(sid.0.cast()))) };
    text
}
