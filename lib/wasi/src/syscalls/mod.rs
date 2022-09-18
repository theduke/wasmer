#![allow(unused, clippy::too_many_arguments, clippy::cognitive_complexity)]

pub mod types {
    pub use wasmer_wasi_types::*;
}

#[cfg(any(
    target_os = "freebsd",
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple"
))]
pub mod unix;
#[cfg(any(target_family = "wasm"))]
pub mod wasm;
#[cfg(any(target_os = "windows"))]
pub mod windows;

pub mod legacy;

use self::types::*;
use crate::state::{bus_error_into_wasi_err, wasi_error_into_bus_err, InodeHttpSocketType, WasiFutex, bus_write_rights, bus_read_rights, WasiBusCall, WasiThreadContext, WasiParkingLot, WasiDummyWaker};
use crate::utils::map_io_err;
use crate::{WasiBusProcessId, WasiEnvInner, import_object_for_all_wasi_versions, WasiFunctionEnv, current_caller_id, DEFAULT_STACK_SIZE};
use crate::{
    mem_error_to_wasi,
    state::{
        self, fs_error_into_wasi_err, iterate_poll_events, net_error_into_wasi_err, poll,
        virtual_file_type_to_wasi_file_type, Fd, Inode, InodeSocket, InodeSocketKind, InodeVal,
        Kind, PollEvent, PollEventBuilder, WasiPipe, WasiState, MAX_SYMLINKS,
    },
    WasiEnv, WasiError, WasiThreadId,
};
use bytes::Bytes;
use cooked_waker::IntoWaker;
use sha2::Sha256;
use wasmer::vm::VMMemory;
use std::borrow::{Borrow, Cow};
use std::cell::RefCell;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::convert::{Infallible, TryInto};
use std::io::{self, Read, Seek, Write};
use std::mem::transmute;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicU32};
use std::sync::{atomic::Ordering, Mutex};
use std::sync::{mpsc, Arc, Condvar};
use std::task::{Poll, Context};
use std::thread::LocalKey;
use std::time::Duration;
use tracing::{debug, error, trace, warn};
use wasmer::{
    AsStoreMut, FunctionEnvMut, Memory, Memory32, Memory64, MemorySize, RuntimeError, Value,
    WasmPtr, WasmSlice, FunctionEnv, Instance, Module, Extern, MemoryView, TypedFunction, Store, Pages, Global, AsStoreRef,
    MemoryAccessError, OnCalledAction, MemoryError
};
use wasmer_vbus::{FileDescriptor, StdioMode, BusDataFormat, BusInvocationEvent};
use wasmer_vfs::{FsError, VirtualFile};
use wasmer_vnet::{SocketHttpRequest, StreamSecurity};
use wasmer_types::LinearMemory;

#[cfg(any(
    target_os = "freebsd",
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple"
))]
pub use unix::*;

#[cfg(any(target_os = "windows"))]
pub use windows::*;

#[cfg(any(target_family = "wasm"))]
pub use wasm::*;

fn to_offset<M: MemorySize>(offset: usize) -> Result<M::Offset, __wasi_errno_t> {
    let ret: M::Offset = offset.try_into().map_err(|_| __WASI_EINVAL)?;
    Ok(ret)
}

fn from_offset<M: MemorySize>(offset: M::Offset) -> Result<usize, __wasi_errno_t> {
    let ret: usize = offset.try_into().map_err(|_| __WASI_EINVAL)?;
    Ok(ret)
}

fn write_bytes_inner<T: Write, M: MemorySize>(
    mut write_loc: T,
    memory: &MemoryView,
    iovs_arr_cell: WasmSlice<__wasi_ciovec_t<M>>,
) -> Result<usize, __wasi_errno_t> {
    let mut bytes_written = 0usize;
    for iov in iovs_arr_cell.iter() {
        let iov_inner = iov.read().map_err(mem_error_to_wasi)?;
        let bytes = WasmPtr::<u8, M>::new(iov_inner.buf)
            .slice(memory, iov_inner.buf_len)
            .map_err(mem_error_to_wasi)?;
        let bytes = bytes.read_to_vec().map_err(mem_error_to_wasi)?;
        write_loc.write_all(&bytes).map_err(map_io_err)?;

        bytes_written += from_offset::<M>(iov_inner.buf_len)?;
    }
    Ok(bytes_written)
}

pub(crate) fn write_bytes<T: Write, M: MemorySize>(
    mut write_loc: T,
    memory: &MemoryView,
    iovs_arr: WasmSlice<__wasi_ciovec_t<M>>,
) -> Result<usize, __wasi_errno_t> {
    let result = write_bytes_inner::<_, M>(&mut write_loc, memory, iovs_arr);
    write_loc.flush();
    result
}

pub(crate) fn read_bytes<T: Read, M: MemorySize>(
    mut reader: T,
    memory: &MemoryView,
    iovs_arr: WasmSlice<__wasi_iovec_t<M>>,
) -> Result<usize, __wasi_errno_t> {
    let mut bytes_read = 0usize;

    // We allocate the raw_bytes first once instead of
    // N times in the loop.
    let mut raw_bytes: Vec<u8> = vec![0; 1024];

    for iov in iovs_arr.iter() {
        let iov_inner = iov.read().map_err(mem_error_to_wasi)?;
        raw_bytes.clear();
        let to_read = from_offset::<M>(iov_inner.buf_len)?;
        raw_bytes.resize(to_read, 0);
        let has_read = reader.read(&mut raw_bytes).map_err(map_io_err)?;

        let buf = WasmPtr::<u8, M>::new(iov_inner.buf)
            .slice(memory, iov_inner.buf_len)
            .map_err(mem_error_to_wasi)?;
        buf.write_slice(&raw_bytes).map_err(mem_error_to_wasi)?;
        bytes_read += has_read;
        if has_read != to_read {
            return Ok(bytes_read);
        }
    }
    Ok(bytes_read)
}

/// checks that `rights_check_set` is a subset of `rights_set`
fn has_rights(rights_set: __wasi_rights_t, rights_check_set: __wasi_rights_t) -> bool {
    rights_set | rights_check_set == rights_set
}

fn __sock_actor<T, F>(
    ctx: &FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    rights: __wasi_rights_t,
    actor: F,
) -> Result<T, __wasi_errno_t>
where
    F: FnOnce(&crate::state::InodeSocket) -> Result<T, __wasi_errno_t>,
{
    let env = ctx.data();
    let (_, state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);

    let fd_entry = state.fs.get_fd(sock)?;
    let ret = {
        if rights != 0 && !has_rights(fd_entry.rights, rights) {
            return Err(__WASI_EACCES);
        }

        let inode_idx = fd_entry.inode;
        let inode = &inodes.arena[inode_idx];

        let mut guard = inode.read();
        match guard.deref() {
            Kind::Socket { socket } => actor(socket)?,
            _ => {
                return Err(__WASI_ENOTSOCK);
            }
        }
    };

    Ok(ret)
}

fn __sock_actor_mut<T, F>(
    ctx: &FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    rights: __wasi_rights_t,
    actor: F,
) -> Result<T, __wasi_errno_t>
where
    F: FnOnce(&mut crate::state::InodeSocket) -> Result<T, __wasi_errno_t>,
{
    let env = ctx.data();
    let (_, state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);

    let fd_entry = state.fs.get_fd(sock)?;
    let ret = {
        if rights != 0 && !has_rights(fd_entry.rights, rights) {
            return Err(__WASI_EACCES);
        }

        let inode_idx = fd_entry.inode;
        let inode = &inodes.arena[inode_idx];

        let mut guard = inode.write();
        match guard.deref_mut() {
            Kind::Socket { socket } => actor(socket)?,
            _ => {
                return Err(__WASI_ENOTSOCK);
            }
        }
    };

    Ok(ret)
}

fn __sock_upgrade<F>(
    ctx: &FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    rights: __wasi_rights_t,
    actor: F,
) -> Result<(), __wasi_errno_t>
where
    F: FnOnce(
        &mut crate::state::InodeSocket,
    ) -> Result<Option<crate::state::InodeSocket>, __wasi_errno_t>,
{
    let env = ctx.data();
    let (_, state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);

    let fd_entry = state.fs.get_fd(sock)?;
    if rights != 0 && !has_rights(fd_entry.rights, rights) {
        return Err(__WASI_EACCES);
    }

    let inode_idx = fd_entry.inode;
    let inode = &inodes.arena[inode_idx];

    let mut guard = inode.write();
    match guard.deref_mut() {
        Kind::Socket { socket } => {
            let new_socket = actor(socket)?;

            if let Some(mut new_socket) = new_socket {
                std::mem::swap(socket, &mut new_socket);
            }
        }
        _ => {
            return Err(__WASI_ENOTSOCK);
        }
    }

    Ok(())
}

#[must_use]
fn write_buffer_array<M: MemorySize>(
    memory: &MemoryView,
    from: &[Vec<u8>],
    ptr_buffer: WasmPtr<WasmPtr<u8, M>, M>,
    buffer: WasmPtr<u8, M>,
) -> __wasi_errno_t {
    let ptrs = wasi_try_mem!(ptr_buffer.slice(memory, wasi_try!(to_offset::<M>(from.len()))));

    let mut current_buffer_offset = 0usize;
    for ((i, sub_buffer), ptr) in from.iter().enumerate().zip(ptrs.iter()) {
        trace!("ptr: {:?}, subbuffer: {:?}", ptr, sub_buffer);
        let mut buf_offset = buffer.offset();
        buf_offset += wasi_try!(to_offset::<M>(current_buffer_offset));
        let new_ptr = WasmPtr::new(buf_offset);
        wasi_try_mem!(ptr.write(new_ptr));

        let data =
            wasi_try_mem!(new_ptr.slice(memory, wasi_try!(to_offset::<M>(sub_buffer.len()))));
        wasi_try_mem!(data.write_slice(sub_buffer));
        wasi_try_mem!(wasi_try_mem!(
            new_ptr.add_offset(wasi_try!(to_offset::<M>(sub_buffer.len())))
        )
        .write(memory, 0));

        current_buffer_offset += sub_buffer.len() + 1;
    }

    __WASI_ESUCCESS
}

fn get_current_time_in_nanos() -> Result<__wasi_timestamp_t, __wasi_errno_t> {
    let now = std::time::SystemTime::now();
    let duration = now
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_err(|_| __WASI_EIO)?;
    Ok(duration.as_nanos() as __wasi_timestamp_t)
}

/// ### `args_get()`
/// Read command-line argument data.
/// The sizes of the buffers should match that returned by [`args_sizes_get()`](#args_sizes_get).
/// Inputs:
/// - `char **argv`
///     A pointer to a buffer to write the argument pointers.
/// - `char *argv_buf`
///     A pointer to a buffer to write the argument string data.
///
pub fn args_get<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    argv: WasmPtr<WasmPtr<u8, M>, M>,
    argv_buf: WasmPtr<u8, M>,
) -> __wasi_errno_t {
    debug!("wasi::args_get");
    let env = ctx.data();
    let (memory, mut state) = env.get_memory_and_wasi_state(&ctx, 0);

    let result = write_buffer_array(&memory, &*state.args, argv, argv_buf);

    debug!(
        "=> args:\n{}",
        state
            .args
            .iter()
            .enumerate()
            .map(|(i, v)| format!("{:>20}: {}", i, ::std::str::from_utf8(v).unwrap()))
            .collect::<Vec<String>>()
            .join("\n")
    );

    result
}

/// ### `args_sizes_get()`
/// Return command-line argument data sizes.
/// Outputs:
/// - `size_t *argc`
///     The number of arguments.
/// - `size_t *argv_buf_size`
///     The size of the argument string data.
pub fn args_sizes_get<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    argc: WasmPtr<M::Offset, M>,
    argv_buf_size: WasmPtr<M::Offset, M>,
) -> __wasi_errno_t {
    debug!("wasi::args_sizes_get");
    let env = ctx.data();
    let (memory, mut state) = env.get_memory_and_wasi_state(&ctx, 0);

    let argc = argc.deref(&memory);
    let argv_buf_size = argv_buf_size.deref(&memory);

    let argc_val: M::Offset = wasi_try!(state.args.len().try_into().map_err(|_| __WASI_EOVERFLOW));
    let argv_buf_size_val: usize = state.args.iter().map(|v| v.len() + 1).sum();
    let argv_buf_size_val: M::Offset =
        wasi_try!(argv_buf_size_val.try_into().map_err(|_| __WASI_EOVERFLOW));
    wasi_try_mem!(argc.write(argc_val));
    wasi_try_mem!(argv_buf_size.write(argv_buf_size_val));

    debug!("=> argc={}, argv_buf_size={}", argc_val, argv_buf_size_val);

    __WASI_ESUCCESS
}

/// ### `clock_res_get()`
/// Get the resolution of the specified clock
/// Input:
/// - `__wasi_clockid_t clock_id`
///     The ID of the clock to get the resolution of
/// Output:
/// - `__wasi_timestamp_t *resolution`
///     The resolution of the clock in nanoseconds
pub fn clock_res_get<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    clock_id: __wasi_clockid_t,
    resolution: WasmPtr<__wasi_timestamp_t, M>,
) -> __wasi_errno_t {
    trace!("wasi::clock_res_get");
    let env = ctx.data();
    let memory = env.memory_view(&ctx);

    let out_addr = resolution.deref(&memory);
    let t_out = wasi_try!(platform_clock_res_get(clock_id, out_addr));
    wasi_try_mem!(resolution.write(&memory, t_out as __wasi_timestamp_t));
    __WASI_ESUCCESS
}

/// ### `clock_time_get()`
/// Get the time of the specified clock
/// Inputs:
/// - `__wasi_clockid_t clock_id`
///     The ID of the clock to query
/// - `__wasi_timestamp_t precision`
///     The maximum amount of error the reading may have
/// Output:
/// - `__wasi_timestamp_t *time`
///     The value of the clock in nanoseconds
pub fn clock_time_get<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    clock_id: __wasi_clockid_t,
    precision: __wasi_timestamp_t,
    time: WasmPtr<__wasi_timestamp_t, M>,
) -> __wasi_errno_t {
    trace!(
        "wasi::clock_time_get clock_id: {}, precision: {}",
        clock_id, precision
    );
    let env = ctx.data();
    let memory = env.memory_view(&ctx);

    let mut t_out = wasi_try!(platform_clock_time_get(clock_id, precision));

    {
        let guard = env.state.clock_offset.lock().unwrap();
        if let Some(offset) = guard.get(&clock_id) {
            t_out += *offset;
        }
    };

    wasi_try_mem!(time.write(&memory, t_out as __wasi_timestamp_t));

    let result = __WASI_ESUCCESS;
    trace!(
        "time: {} => {}",
        wasi_try_mem!(time.deref(&memory).read()),
        result
    );
    result
}

/// ### `clock_time_set()`
/// Set the time of the specified clock
/// Inputs:
/// - `__wasi_clockid_t clock_id`
///     The ID of the clock to query
/// - `__wasi_timestamp_t *time`
///     The value of the clock in nanoseconds
pub fn clock_time_set<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    clock_id: __wasi_clockid_t,
    time: __wasi_timestamp_t,
) -> __wasi_errno_t {
    trace!(
        "wasi::clock_time_set clock_id: {}, time: {}",
        clock_id, time
    );
    let env = ctx.data();
    let memory = env.memory_view(&ctx);

    let precision = 1 as __wasi_timestamp_t;
    let t_now = wasi_try!(platform_clock_time_get(clock_id, precision));
    let t_now = t_now as i64;

    let t_target = time as i64;
    let t_offset = t_target - t_now;
    
    let mut guard = env.state.clock_offset.lock().unwrap();
    guard.insert(clock_id, t_offset);

    __WASI_ESUCCESS
}

/// ### `environ_get()`
/// Read environment variable data.
/// The sizes of the buffers should match that returned by [`environ_sizes_get()`](#environ_sizes_get).
/// Inputs:
/// - `char **environ`
///     A pointer to a buffer to write the environment variable pointers.
/// - `char *environ_buf`
///     A pointer to a buffer to write the environment variable string data.
pub fn environ_get<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    environ: WasmPtr<WasmPtr<u8, M>, M>,
    environ_buf: WasmPtr<u8, M>,
) -> __wasi_errno_t {
    trace!(
        "wasi::environ_get. Environ: {:?}, environ_buf: {:?}",
        environ, environ_buf
    );
    let env = ctx.data();
    let (memory, mut state) = env.get_memory_and_wasi_state(&ctx, 0);
    trace!(" -> State envs: {:?}", state.envs);

    write_buffer_array(&memory, &*state.envs, environ, environ_buf)
}

/// ### `environ_sizes_get()`
/// Return command-line argument data sizes.
/// Outputs:
/// - `size_t *environ_count`
///     The number of environment variables.
/// - `size_t *environ_buf_size`
///     The size of the environment variable string data.
pub fn environ_sizes_get<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    environ_count: WasmPtr<M::Offset, M>,
    environ_buf_size: WasmPtr<M::Offset, M>,
) -> __wasi_errno_t {
    trace!("wasi::environ_sizes_get");
    let env = ctx.data();
    let (memory, mut state) = env.get_memory_and_wasi_state(&ctx, 0);

    let environ_count = environ_count.deref(&memory);
    let environ_buf_size = environ_buf_size.deref(&memory);

    let env_var_count: M::Offset =
        wasi_try!(state.envs.len().try_into().map_err(|_| __WASI_EOVERFLOW));
    let env_buf_size: usize = state.envs.iter().map(|v| v.len() + 1).sum();
    let env_buf_size: M::Offset = wasi_try!(env_buf_size.try_into().map_err(|_| __WASI_EOVERFLOW));
    wasi_try_mem!(environ_count.write(env_var_count));
    wasi_try_mem!(environ_buf_size.write(env_buf_size));

    trace!(
        "env_var_count: {}, env_buf_size: {}",
        env_var_count,
        env_buf_size
    );

    __WASI_ESUCCESS
}

/// ### `fd_advise()`
/// Advise the system about how a file will be used
/// Inputs:
/// - `__wasi_fd_t fd`
///     The file descriptor the advice applies to
/// - `__wasi_filesize_t offset`
///     The offset from which the advice applies
/// - `__wasi_filesize_t len`
///     The length from the offset to which the advice applies
/// - `__wasi_advice_t advice`
///     The advice to give
pub fn fd_advise(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    offset: __wasi_filesize_t,
    len: __wasi_filesize_t,
    advice: __wasi_advice_t,
) -> __wasi_errno_t {
    debug!("wasi::fd_advise: fd={}", fd);

    // this is used for our own benefit, so just returning success is a valid
    // implementation for now
    __WASI_ESUCCESS
}

/// ### `fd_allocate`
/// Allocate extra space for a file descriptor
/// Inputs:
/// - `__wasi_fd_t fd`
///     The file descriptor to allocate for
/// - `__wasi_filesize_t offset`
///     The offset from the start marking the beginning of the allocation
/// - `__wasi_filesize_t len`
///     The length from the offset marking the end of the allocation
pub fn fd_allocate(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    offset: __wasi_filesize_t,
    len: __wasi_filesize_t,
) -> __wasi_errno_t {
    debug!("wasi::fd_allocate");
    let env = ctx.data();
    let (_, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);
    let fd_entry = wasi_try!(state.fs.get_fd(fd));
    let inode = fd_entry.inode;

    if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_ALLOCATE) {
        return __WASI_EACCES;
    }
    let new_size = wasi_try!(offset.checked_add(len).ok_or(__WASI_EINVAL));
    {
        let mut guard = inodes.arena[inode].write();
        match guard.deref_mut() {
            Kind::File { handle, .. } => {
                if let Some(handle) = handle {
                    wasi_try!(handle.set_len(new_size).map_err(fs_error_into_wasi_err));
                } else {
                    return __WASI_EBADF;
                }
            }
            Kind::Socket { .. } => return __WASI_EBADF,
            Kind::Pipe { .. } => return __WASI_EBADF,
            Kind::Buffer { buffer } => {
                buffer.resize(new_size as usize, 0);
            }
            Kind::Symlink { .. } => return __WASI_EBADF,
            Kind::EventNotifications { .. } => return __WASI_EBADF,
            Kind::Dir { .. } | Kind::Root { .. } => return __WASI_EISDIR,
        }
    }
    inodes.arena[inode].stat.write().unwrap().st_size = new_size;
    debug!("New file size: {}", new_size);

    __WASI_ESUCCESS
}

/// ### `fd_close()`
/// Close an open file descriptor
/// Inputs:
/// - `__wasi_fd_t fd`
///     A file descriptor mapping to an open file to close
/// Errors:
/// - `__WASI_EISDIR`
///     If `fd` is a directory
/// - `__WASI_EBADF`
///     If `fd` is invalid or not open
pub fn fd_close(ctx: FunctionEnvMut<'_, WasiEnv>, fd: __wasi_fd_t) -> __wasi_errno_t {
    debug!("wasi::fd_close: fd={}", fd);
    let env = ctx.data();
    let (_, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);

    let fd_entry = wasi_try!(state.fs.get_fd(fd));

    wasi_try!(state.fs.close_fd(inodes.deref(), fd));

    __WASI_ESUCCESS
}

/// ### `fd_datasync()`
/// Synchronize the file data to disk
/// Inputs:
/// - `__wasi_fd_t fd`
///     The file descriptor to sync
pub fn fd_datasync(ctx: FunctionEnvMut<'_, WasiEnv>, fd: __wasi_fd_t) -> __wasi_errno_t {
    debug!("wasi::fd_datasync");
    let env = ctx.data();
    let (_, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);
    let fd_entry = wasi_try!(state.fs.get_fd(fd));
    if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_DATASYNC) {
        return __WASI_EACCES;
    }

    if let Err(e) = state.fs.flush(inodes.deref(), fd) {
        e
    } else {
        __WASI_ESUCCESS
    }
}

/// ### `fd_fdstat_get()`
/// Get metadata of a file descriptor
/// Input:
/// - `__wasi_fd_t fd`
///     The file descriptor whose metadata will be accessed
/// Output:
/// - `__wasi_fdstat_t *buf`
///     The location where the metadata will be written
pub fn fd_fdstat_get<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    buf_ptr: WasmPtr<__wasi_fdstat_t, M>,
) -> __wasi_errno_t {
    debug!(
        "wasi::fd_fdstat_get: fd={}, buf_ptr={}",
        fd,
        buf_ptr.offset()
    );
    let env = ctx.data();
    let (memory, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);
    let stat = wasi_try!(state.fs.fdstat(inodes.deref(), fd));

    let buf = buf_ptr.deref(&memory);

    wasi_try_mem!(buf.write(stat));

    __WASI_ESUCCESS
}

/// ### `fd_fdstat_set_flags()`
/// Set file descriptor flags for a file descriptor
/// Inputs:
/// - `__wasi_fd_t fd`
///     The file descriptor to apply the new flags to
/// - `__wasi_fdflags_t flags`
///     The flags to apply to `fd`
pub fn fd_fdstat_set_flags(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    flags: __wasi_fdflags_t,
) -> __wasi_errno_t {
    debug!("wasi::fd_fdstat_set_flags");
    let env = ctx.data();
    let (_, mut state) = env.get_memory_and_wasi_state(&ctx, 0);
    let mut fd_map = state.fs.fd_map.write().unwrap();
    let fd_entry = wasi_try!(fd_map.get_mut(&fd).ok_or(__WASI_EBADF));

    if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_FDSTAT_SET_FLAGS) {
        return __WASI_EACCES;
    }

    fd_entry.flags = flags;
    __WASI_ESUCCESS
}

/// ### `fd_fdstat_set_rights()`
/// Set the rights of a file descriptor.  This can only be used to remove rights
/// Inputs:
/// - `__wasi_fd_t fd`
///     The file descriptor to apply the new rights to
/// - `__wasi_rights_t fs_rights_base`
///     The rights to apply to `fd`
/// - `__wasi_rights_t fs_rights_inheriting`
///     The inheriting rights to apply to `fd`
pub fn fd_fdstat_set_rights(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    fs_rights_base: __wasi_rights_t,
    fs_rights_inheriting: __wasi_rights_t,
) -> __wasi_errno_t {
    debug!("wasi::fd_fdstat_set_rights");
    let env = ctx.data();
    let (_, mut state) = env.get_memory_and_wasi_state(&ctx, 0);
    let mut fd_map = state.fs.fd_map.write().unwrap();
    let fd_entry = wasi_try!(fd_map.get_mut(&fd).ok_or(__WASI_EBADF));

    // ensure new rights are a subset of current rights
    if fd_entry.rights | fs_rights_base != fd_entry.rights
        || fd_entry.rights_inheriting | fs_rights_inheriting != fd_entry.rights_inheriting
    {
        return __WASI_ENOTCAPABLE;
    }

    fd_entry.rights = fs_rights_base;
    fd_entry.rights_inheriting = fs_rights_inheriting;

    __WASI_ESUCCESS
}

/// ### `fd_filestat_get()`
/// Get the metadata of an open file
/// Input:
/// - `__wasi_fd_t fd`
///     The open file descriptor whose metadata will be read
/// Output:
/// - `__wasi_filestat_t *buf`
///     Where the metadata from `fd` will be written
pub fn fd_filestat_get<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    buf: WasmPtr<__wasi_filestat_t, M>,
) -> __wasi_errno_t {
    fd_filestat_get_internal(&mut ctx, fd, buf)
}

/// ### `fd_filestat_get()`
/// Get the metadata of an open file
/// Input:
/// - `__wasi_fd_t fd`
///     The open file descriptor whose metadata will be read
/// Output:
/// - `__wasi_filestat_t *buf`
///     Where the metadata from `fd` will be written
pub(crate) fn fd_filestat_get_internal<M: MemorySize>(
    ctx: &mut FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    buf: WasmPtr<__wasi_filestat_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::fd_filestat_get");
    let env = ctx.data();
    let (memory, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);
    let fd_entry = wasi_try!(state.fs.get_fd(fd));
    if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_FILESTAT_GET) {
        return __WASI_EACCES;
    }

    let stat = wasi_try!(state.fs.filestat_fd(inodes.deref(), fd));

    let buf = buf.deref(&memory);
    wasi_try_mem!(buf.write(stat));

    __WASI_ESUCCESS
}

/// ### `fd_filestat_set_size()`
/// Change the size of an open file, zeroing out any new bytes
/// Inputs:
/// - `__wasi_fd_t fd`
///     File descriptor to adjust
/// - `__wasi_filesize_t st_size`
///     New size that `fd` will be set to
pub fn fd_filestat_set_size(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    st_size: __wasi_filesize_t,
) -> __wasi_errno_t {
    debug!("wasi::fd_filestat_set_size");
    let env = ctx.data();
    let (_, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);
    let fd_entry = wasi_try!(state.fs.get_fd(fd));
    let inode = fd_entry.inode;

    if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_FILESTAT_SET_SIZE) {
        return __WASI_EACCES;
    }

    {
        let mut guard = inodes.arena[inode].write();
        match guard.deref_mut() {
            Kind::File { handle, .. } => {
                if let Some(handle) = handle {
                    wasi_try!(handle.set_len(st_size).map_err(fs_error_into_wasi_err));
                } else {
                    return __WASI_EBADF;
                }
            }
            Kind::Buffer { buffer } => {
                buffer.resize(st_size as usize, 0);
            }
            Kind::Socket { .. } => return __WASI_EBADF,
            Kind::Pipe { .. } => return __WASI_EBADF,
            Kind::Symlink { .. } => return __WASI_EBADF,
            Kind::EventNotifications { .. } => return __WASI_EBADF,
            Kind::Dir { .. } | Kind::Root { .. } => return __WASI_EISDIR,
        }
    }
    inodes.arena[inode].stat.write().unwrap().st_size = st_size;

    __WASI_ESUCCESS
}

/// ### `fd_filestat_set_times()`
/// Set timestamp metadata on a file
/// Inputs:
/// - `__wasi_timestamp_t st_atim`
///     Last accessed time
/// - `__wasi_timestamp_t st_mtim`
///     Last modified time
/// - `__wasi_fstflags_t fst_flags`
///     Bit-vector for controlling which times get set
pub fn fd_filestat_set_times(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    st_atim: __wasi_timestamp_t,
    st_mtim: __wasi_timestamp_t,
    fst_flags: __wasi_fstflags_t,
) -> __wasi_errno_t {
    debug!("wasi::fd_filestat_set_times");
    let env = ctx.data();
    let (_, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);
    let fd_entry = wasi_try!(state.fs.get_fd(fd));

    if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_FILESTAT_SET_TIMES) {
        return __WASI_EACCES;
    }

    if (fst_flags & __WASI_FILESTAT_SET_ATIM != 0 && fst_flags & __WASI_FILESTAT_SET_ATIM_NOW != 0)
        || (fst_flags & __WASI_FILESTAT_SET_MTIM != 0
            && fst_flags & __WASI_FILESTAT_SET_MTIM_NOW != 0)
    {
        return __WASI_EINVAL;
    }

    let inode_idx = fd_entry.inode;
    let inode = &inodes.arena[inode_idx];

    if fst_flags & __WASI_FILESTAT_SET_ATIM != 0 || fst_flags & __WASI_FILESTAT_SET_ATIM_NOW != 0 {
        let time_to_set = if fst_flags & __WASI_FILESTAT_SET_ATIM != 0 {
            st_atim
        } else {
            wasi_try!(get_current_time_in_nanos())
        };
        inode.stat.write().unwrap().st_atim = time_to_set;
    }

    if fst_flags & __WASI_FILESTAT_SET_MTIM != 0 || fst_flags & __WASI_FILESTAT_SET_MTIM_NOW != 0 {
        let time_to_set = if fst_flags & __WASI_FILESTAT_SET_MTIM != 0 {
            st_mtim
        } else {
            wasi_try!(get_current_time_in_nanos())
        };
        inode.stat.write().unwrap().st_mtim = time_to_set;
    }

    __WASI_ESUCCESS
}

/// ### `fd_pread()`
/// Read from the file at the given offset without updating the file cursor.
/// This acts like a stateless version of Seek + Read
/// Inputs:
/// - `__wasi_fd_t fd`
///     The file descriptor to read the data with
/// - `const __wasi_iovec_t* iovs'
///     Vectors where the data will be stored
/// - `size_t iovs_len`
///     The number of vectors to store the data into
/// - `__wasi_filesize_t offset`
///     The file cursor to use: the starting position from which data will be read
/// Output:
/// - `size_t nread`
///     The number of bytes read
pub fn fd_pread<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    iovs: WasmPtr<__wasi_iovec_t<M>, M>,
    iovs_len: M::Offset,
    offset: __wasi_filesize_t,
    nread: WasmPtr<M::Offset, M>,
) -> Result<__wasi_errno_t, WasiError> {
    trace!("wasi::fd_pread: fd={}, offset={}", fd, offset);
    let env = ctx.data();
    let (memory, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);

    let iovs = wasi_try_mem_ok!(iovs.slice(&memory, iovs_len));
    let nread_ref = nread.deref(&memory);

    let fd_entry = wasi_try_ok!(state.fs.get_fd(fd));
    let bytes_read = match fd {
        __WASI_STDIN_FILENO => {
            let mut guard = wasi_try_ok!(
                inodes
                    .stdin_mut(&state.fs.fd_map)
                    .map_err(fs_error_into_wasi_err),
                env
            );
            if let Some(ref mut stdin) = guard.deref_mut() {
                wasi_try_ok!(read_bytes(stdin, &memory, iovs), env)
            } else {
                return Ok(__WASI_EBADF);
            }
        }
        __WASI_STDOUT_FILENO => return Ok(__WASI_EINVAL),
        __WASI_STDERR_FILENO => return Ok(__WASI_EINVAL),
        _ => {
            let inode = fd_entry.inode;

            if !(has_rights(fd_entry.rights, __WASI_RIGHT_FD_READ)
                && has_rights(fd_entry.rights, __WASI_RIGHT_FD_SEEK))
            {
                debug!(
                    "Invalid rights on {:X}: expected READ and SEEK",
                    fd_entry.rights
                );
                return Ok(__WASI_EACCES);
            }
            let mut guard = inodes.arena[inode].write();
            match guard.deref_mut() {
                Kind::File { handle, .. } => {
                    if let Some(h) = handle {
                        wasi_try_ok!(
                            h.seek(std::io::SeekFrom::Start(offset as u64))
                                .map_err(map_io_err),
                            env
                        );
                        wasi_try_ok!(read_bytes(h, &memory, iovs), env)
                    } else {
                        return Ok(__WASI_EINVAL);
                    }
                }
                Kind::Socket { socket } => {
                    wasi_try_ok!(socket.recv(&memory, iovs), env)
                }
                Kind::Pipe { pipe } => {
                    wasi_try_ok!(pipe.recv(&memory, iovs), env)
                }
                Kind::EventNotifications { .. } => return Ok(__WASI_EINVAL),
                Kind::Dir { .. } | Kind::Root { .. } => return Ok(__WASI_EISDIR),
                Kind::Symlink { .. } => unimplemented!("Symlinks in wasi::fd_pread"),
                Kind::Buffer { buffer } => {
                    wasi_try_ok!(
                        read_bytes(&buffer[(offset as usize)..], &memory, iovs),
                        env
                    )
                }
            }
        }
    };

    let bytes_read: M::Offset = wasi_try_ok!(bytes_read.try_into().map_err(|_| __WASI_EOVERFLOW));
    wasi_try_mem_ok!(nread_ref.write(bytes_read));
    debug!("Success: {} bytes read", bytes_read);
    Ok(__WASI_ESUCCESS)
}

/// ### `fd_prestat_get()`
/// Get metadata about a preopened file descriptor
/// Input:
/// - `__wasi_fd_t fd`
///     The preopened file descriptor to query
/// Output:
/// - `__wasi_prestat *buf`
///     Where the metadata will be written
pub fn fd_prestat_get<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    buf: WasmPtr<__wasi_prestat_t, M>,
) -> __wasi_errno_t {
    trace!("wasi::fd_prestat_get: fd={}", fd);
    let env = ctx.data();
    let (memory, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);

    let prestat_ptr = buf.deref(&memory);
    wasi_try_mem!(prestat_ptr.write(wasi_try!(
        state.fs.prestat_fd(inodes.deref(), fd)
            .map_err(|code| {
                debug!("fd_prestat_get failed (fd={}) - errno={}", fd, code);
                code
            })
    )));

    __WASI_ESUCCESS
}

pub fn fd_prestat_dir_name<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    path: WasmPtr<u8, M>,
    path_len: M::Offset,
) -> __wasi_errno_t {
    trace!(
        "wasi::fd_prestat_dir_name: fd={}, path_len={}",
        fd,
        path_len
    );
    let env = ctx.data();
    let (memory, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);
    let path_chars = wasi_try_mem!(path.slice(&memory, path_len));

    let real_inode = wasi_try!(state.fs.get_fd_inode(fd));
    let inode_val = &inodes.arena[real_inode];

    // check inode-val.is_preopened?

    trace!("=> inode: {:?}", inode_val);
    let guard = inode_val.read();
    match guard.deref() {
        Kind::Dir { .. } | Kind::Root { .. } => {
            // TODO: verify this: null termination, etc
            let path_len: u64 = path_len.into();
            if (inode_val.name.len() as u64) < path_len {
                wasi_try_mem!(path_chars
                    .subslice(0..inode_val.name.len() as u64)
                    .write_slice(inode_val.name.as_bytes()));
                wasi_try_mem!(path_chars.index(inode_val.name.len() as u64).write(0));

                trace!("=> result: \"{}\"", inode_val.name);

                __WASI_ESUCCESS
            } else {
                __WASI_EOVERFLOW
            }
        }
        Kind::Symlink { .. }
        | Kind::Buffer { .. }
        | Kind::File { .. }
        | Kind::Socket { .. }
        | Kind::Pipe { .. }
        | Kind::EventNotifications { .. } => __WASI_ENOTDIR,
    }
}

/// ### `fd_pwrite()`
/// Write to a file without adjusting its offset
/// Inputs:
/// - `__wasi_fd_t`
///     File descriptor (opened with writing) to write to
/// - `const __wasi_ciovec_t *iovs`
///     List of vectors to read data from
/// - `u32 iovs_len`
///     Length of data in `iovs`
/// - `__wasi_filesize_t offset`
///     The offset to write at
/// Output:
/// - `u32 *nwritten`
///     Number of bytes written
pub fn fd_pwrite<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    iovs: WasmPtr<__wasi_ciovec_t<M>, M>,
    iovs_len: M::Offset,
    offset: __wasi_filesize_t,
    nwritten: WasmPtr<M::Offset, M>,
) -> Result<__wasi_errno_t, WasiError> {
    trace!("wasi::fd_pwrite");
    // TODO: refactor, this is just copied from `fd_write`...
    let env = ctx.data();
    let (memory, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);
    let iovs_arr = wasi_try_mem_ok!(iovs.slice(&memory, iovs_len));
    let nwritten_ref = nwritten.deref(&memory);

    let fd_entry = wasi_try_ok!(state.fs.get_fd(fd));
    let bytes_written = match fd {
        __WASI_STDIN_FILENO => return Ok(__WASI_EINVAL),
        __WASI_STDOUT_FILENO => {
            let mut guard = wasi_try_ok!(
                inodes
                    .stdout_mut(&state.fs.fd_map)
                    .map_err(fs_error_into_wasi_err),
                env
            );
            if let Some(ref mut stdout) = guard.deref_mut() {
                wasi_try_ok!(write_bytes(stdout, &memory, iovs_arr), env)
            } else {
                return Ok(__WASI_EBADF);
            }
        }
        __WASI_STDERR_FILENO => {
            let mut guard = wasi_try_ok!(
                inodes
                    .stderr_mut(&state.fs.fd_map)
                    .map_err(fs_error_into_wasi_err),
                env
            );
            if let Some(ref mut stderr) = guard.deref_mut() {
                wasi_try_ok!(write_bytes(stderr, &memory, iovs_arr), env)
            } else {
                return Ok(__WASI_EBADF);
            }
        }
        _ => {
            if !(has_rights(fd_entry.rights, __WASI_RIGHT_FD_WRITE)
                && has_rights(fd_entry.rights, __WASI_RIGHT_FD_SEEK))
            {
                return Ok(__WASI_EACCES);
            }

            let inode_idx = fd_entry.inode;
            let inode = &inodes.arena[inode_idx];

            let mut guard = inode.write();
            match guard.deref_mut() {
                Kind::File { handle, .. } => {
                    if let Some(handle) = handle {
                        wasi_try_ok!(
                            handle
                                .seek(std::io::SeekFrom::Start(offset as u64))
                                .map_err(map_io_err),
                            env
                        );
                        wasi_try_ok!(write_bytes(handle, &memory, iovs_arr), env)
                    } else {
                        return Ok(__WASI_EINVAL);
                    }
                }
                Kind::Socket { socket } => {
                    wasi_try_ok!(socket.send(&memory, iovs_arr), env)
                }
                Kind::Pipe { pipe } => {
                    wasi_try_ok!(pipe.send(&memory, iovs_arr), env)
                }
                Kind::Dir { .. } | Kind::Root { .. } => {
                    // TODO: verify
                    return Ok(__WASI_EISDIR);
                }
                Kind::EventNotifications { .. } => return Ok(__WASI_EINVAL),
                Kind::Symlink { .. } => unimplemented!("Symlinks in wasi::fd_pwrite"),
                Kind::Buffer { buffer } => {
                    wasi_try_ok!(
                        write_bytes(&mut buffer[(offset as usize)..], &memory, iovs_arr),
                        env
                    )
                }
            }
        }
    };

    let bytes_written: M::Offset =
        wasi_try_ok!(bytes_written.try_into().map_err(|_| __WASI_EOVERFLOW));
    wasi_try_mem_ok!(nwritten_ref.write(bytes_written));

    Ok(__WASI_ESUCCESS)
}

/// ### `fd_read()`
/// Read data from file descriptor
/// Inputs:
/// - `__wasi_fd_t fd`
///     File descriptor from which data will be read
/// - `const __wasi_iovec_t *iovs`
///     Vectors where data will be stored
/// - `u32 iovs_len`
///     Length of data in `iovs`
/// Output:
/// - `u32 *nread`
///     Number of bytes read
///
pub fn fd_read<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    iovs: WasmPtr<__wasi_iovec_t<M>, M>,
    iovs_len: M::Offset,
    nread: WasmPtr<M::Offset, M>,
) -> Result<__wasi_errno_t, WasiError> {
    trace!("wasi::fd_read: fd={}", fd);
    let mut env = ctx.data();
    let state = env.state.clone();
    let inodes = state.inodes.clone();
    //let iovs_len = if iovs_len > M::Offset::from(1u32) { M::Offset::from(1u32) } else { iovs_len };
    
    let fd_entry = wasi_try_ok!(state.fs.get_fd(fd));
    let bytes_read = {
        let inodes = inodes.read().unwrap();
        match fd {
            __WASI_STDIN_FILENO => {
                let mut guard = wasi_try_ok!(
                    inodes
                        .stdin_mut(&state.fs.fd_map)
                        .map_err(fs_error_into_wasi_err),
                    env
                );
                if let Some(ref mut stdin) = guard.deref_mut() {
                    let mut memory = env.memory_view(&ctx);
                    let iovs_arr = wasi_try_mem_ok!(iovs.slice(&memory, iovs_len));
                    wasi_try_ok!(read_bytes(stdin, &memory, iovs_arr), env)
                } else {
                    return Ok(__WASI_EBADF);
                }
            }
            __WASI_STDOUT_FILENO | __WASI_STDERR_FILENO => return Ok(__WASI_EINVAL),
            _ => {
                if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_READ) {
                    // TODO: figure out the error to return when lacking rights
                    return Ok(__WASI_EACCES);
                }

                let is_non_blocking = fd_entry.flags & __WASI_FDFLAG_NONBLOCK != 0;
                let offset = fd_entry.offset as usize;
                let inode_idx = fd_entry.inode;
                let inode = &inodes.arena[inode_idx];

                let bytes_read = {
                    let mut guard = inode.write();
                    match guard.deref_mut() {
                        Kind::File { handle, .. } => {
                            if let Some(handle) = handle {
                                wasi_try_ok!(
                                    handle
                                        .seek(std::io::SeekFrom::Start(offset as u64))
                                        .map_err(map_io_err),
                                    env
                                );
                                let mut memory = env.memory_view(&ctx);
                                let iovs_arr = wasi_try_mem_ok!(iovs.slice(&memory, iovs_len));
                                wasi_try_ok!(read_bytes(handle, &memory, iovs_arr), env)
                            } else {
                                return Ok(__WASI_EINVAL);
                            }
                        }
                        Kind::Socket { socket } => {
                            let mut memory = env.memory_view(&ctx);
                            let iovs_arr = wasi_try_mem_ok!(iovs.slice(&memory, iovs_len));
                            wasi_try_ok!(socket.recv(&memory, iovs_arr), env)
                        }
                        Kind::Pipe { pipe } => {
                            let mut memory = env.memory_view(&ctx);
                            let iovs_arr = wasi_try_mem_ok!(iovs.slice(&memory, iovs_len));
                            wasi_try_ok!(pipe.recv(&memory, iovs_arr), env)
                        }
                        Kind::Dir { .. } | Kind::Root { .. } => {
                            // TODO: verify
                            return Ok(__WASI_EISDIR);
                        }
                        Kind::EventNotifications {
                            counter,
                            is_semaphore,
                            wakers,
                        } => {
                            let counter = Arc::clone(counter);
                            let is_semaphore: bool = *is_semaphore;
                            let wakers = Arc::clone(wakers);
                            drop(guard);
                            drop(inodes);

                            let (tx, rx) = mpsc::channel();
                            {
                                let mut guard = wakers.lock().unwrap();
                                guard.push_front(tx);
                            }

                            let ret;
                            loop {
                                let val = counter.load(Ordering::Acquire);
                                if val > 0 {
                                    let new_val = if is_semaphore { val - 1 } else { 0 };
                                    if counter
                                        .compare_exchange(
                                            val,
                                            new_val,
                                            Ordering::AcqRel,
                                            Ordering::Acquire,
                                        )
                                        .is_ok()
                                    {
                                        let mut memory = env.memory_view(&ctx);
                                        let reader = val.to_ne_bytes();
                                        let iovs_arr = wasi_try_mem_ok!(iovs.slice(&memory, iovs_len));
                                        ret = wasi_try_ok!(
                                            read_bytes(&reader[..], &memory, iovs_arr),
                                            env
                                        );
                                        break;
                                    } else {
                                        continue;
                                    }
                                }

                                // If its none blocking then exit
                                if is_non_blocking {
                                    return Ok(__WASI_EAGAIN);
                                }

                                // Yield for a fixed period of time and then check again
                                env.yield_now()?;
                                if rx.recv_timeout(Duration::from_millis(5)).is_err() {
                                    env.clone().sleep(&mut ctx, Duration::from_millis(5))?;
                                }
                                env = ctx.data();
                            }
                            ret
                        }
                        Kind::Symlink { .. } => unimplemented!("Symlinks in wasi::fd_read"),
                        Kind::Buffer { buffer } => {
                            let mut memory = env.memory_view(&ctx);
                            let iovs_arr = wasi_try_mem_ok!(iovs.slice(&memory, iovs_len));
                            wasi_try_ok!(read_bytes(&buffer[offset..], &memory, iovs_arr), env)
                        }
                    }
                };

                // reborrow
                let mut fd_map = state.fs.fd_map.write().unwrap();
                let fd_entry = wasi_try_ok!(fd_map.get_mut(&fd).ok_or(__WASI_EBADF));
                fd_entry.offset += bytes_read as u64;

                bytes_read
            }
        }
    };

    let bytes_read: M::Offset = wasi_try_ok!(bytes_read.try_into().map_err(|_| __WASI_EOVERFLOW));
    trace!("wasi::fd_read: bytes_read={}", bytes_read);
    
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let nread_ref = nread.deref(&memory);
    wasi_try_mem_ok!(nread_ref.write(bytes_read));

    Ok(__WASI_ESUCCESS)
}

/// ### `fd_readdir()`
/// Read data from directory specified by file descriptor
/// Inputs:
/// - `__wasi_fd_t fd`
///     File descriptor from which directory data will be read
/// - `void *buf`
///     Buffer where directory entries are stored
/// - `u32 buf_len`
///     Length of data in `buf`
/// - `__wasi_dircookie_t cookie`
///     Where the directory reading should start from
/// Output:
/// - `u32 *bufused`
///     The Number of bytes stored in `buf`; if less than `buf_len` then entire
///     directory has been read
pub fn fd_readdir<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    buf: WasmPtr<u8, M>,
    buf_len: M::Offset,
    cookie: __wasi_dircookie_t,
    bufused: WasmPtr<M::Offset, M>,
) -> __wasi_errno_t {
    trace!("wasi::fd_readdir");
    let env = ctx.data();
    let (memory, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);
    // TODO: figure out how this is supposed to work;
    // is it supposed to pack the buffer full every time until it can't? or do one at a time?

    let buf_arr = wasi_try_mem!(buf.slice(&memory, buf_len));
    let bufused_ref = bufused.deref(&memory);
    let working_dir = wasi_try!(state.fs.get_fd(fd));
    let mut cur_cookie = cookie;
    let mut buf_idx = 0usize;

    let entries: Vec<(String, u8, u64)> = {
        let guard = inodes.arena[working_dir.inode].read();
        match guard.deref() {
            Kind::Dir { path, entries, .. } => {
                debug!("Reading dir {:?}", path);
                // TODO: refactor this code
                // we need to support multiple calls,
                // simple and obviously correct implementation for now:
                // maintain consistent order via lexacographic sorting
                let fs_info = wasi_try!(wasi_try!(state.fs_read_dir(path))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(fs_error_into_wasi_err));
                let mut entry_vec = wasi_try!(fs_info
                    .into_iter()
                    .map(|entry| {
                        let filename = entry.file_name().to_string_lossy().to_string();
                        debug!("Getting file: {:?}", filename);
                        let filetype = virtual_file_type_to_wasi_file_type(
                            entry.file_type().map_err(fs_error_into_wasi_err)?,
                        );
                        Ok((
                            filename, filetype, 0, // TODO: inode
                        ))
                    })
                    .collect::<Result<Vec<(String, u8, u64)>, _>>());
                entry_vec.extend(
                    entries
                        .iter()
                        .filter(|(_, inode)| inodes.arena[**inode].is_preopened)
                        .map(|(name, inode)| {
                            let entry = &inodes.arena[*inode];
                            let stat = entry.stat.read().unwrap();
                            (entry.name.to_string(), stat.st_filetype, stat.st_ino)
                        }),
                );
                // adding . and .. special folders
                // TODO: inode
                entry_vec.push((".".to_string(), __WASI_FILETYPE_DIRECTORY, 0));
                entry_vec.push(("..".to_string(), __WASI_FILETYPE_DIRECTORY, 0));
                entry_vec.sort_by(|a, b| a.0.cmp(&b.0));
                entry_vec
            }
            Kind::Root { entries } => {
                debug!("Reading root");
                let sorted_entries = {
                    let mut entry_vec: Vec<(String, Inode)> =
                        entries.iter().map(|(a, b)| (a.clone(), *b)).collect();
                    entry_vec.sort_by(|a, b| a.0.cmp(&b.0));
                    entry_vec
                };
                sorted_entries
                    .into_iter()
                    .map(|(name, inode)| {
                        let entry = &inodes.arena[inode];
                        let stat = entry.stat.read().unwrap();
                        (format!("/{}", entry.name), stat.st_filetype, stat.st_ino)
                    })
                    .collect()
            }
            Kind::File { .. }
            | Kind::Symlink { .. }
            | Kind::Buffer { .. }
            | Kind::Socket { .. }
            | Kind::Pipe { .. }
            | Kind::EventNotifications { .. } => return __WASI_ENOTDIR,
        }
    };

    for (entry_path_str, wasi_file_type, ino) in entries.iter().skip(cookie as usize) {
        cur_cookie += 1;
        let namlen = entry_path_str.len();
        debug!("Returning dirent for {}", entry_path_str);
        let dirent = __wasi_dirent_t {
            d_next: cur_cookie,
            d_ino: *ino,
            d_namlen: namlen as u32,
            d_type: *wasi_file_type,
        };
        let dirent_bytes = dirent_to_le_bytes(&dirent);
        let buf_len: u64 = buf_len.into();
        let upper_limit = std::cmp::min(
            (buf_len - buf_idx as u64) as usize,
            std::mem::size_of::<__wasi_dirent_t>(),
        );
        for (i, b) in dirent_bytes.iter().enumerate().take(upper_limit) {
            wasi_try_mem!(buf_arr.index((i + buf_idx) as u64).write(*b));
        }
        buf_idx += upper_limit;
        if upper_limit != std::mem::size_of::<__wasi_dirent_t>() {
            break;
        }
        let upper_limit = std::cmp::min((buf_len - buf_idx as u64) as usize, namlen);
        for (i, b) in entry_path_str.bytes().take(upper_limit).enumerate() {
            wasi_try_mem!(buf_arr.index((i + buf_idx) as u64).write(b));
        }
        buf_idx += upper_limit;
        if upper_limit != namlen {
            break;
        }
    }

    let buf_idx: M::Offset = wasi_try!(buf_idx.try_into().map_err(|_| __WASI_EOVERFLOW));
    wasi_try_mem!(bufused_ref.write(buf_idx));
    __WASI_ESUCCESS
}

/// ### `fd_renumber()`
/// Atomically copy file descriptor
/// Inputs:
/// - `__wasi_fd_t from`
///     File descriptor to copy
/// - `__wasi_fd_t to`
///     Location to copy file descriptor to
pub fn fd_renumber(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    from: __wasi_fd_t,
    to: __wasi_fd_t,
) -> __wasi_errno_t {
    debug!("wasi::fd_renumber: from={}, to={}", from, to);
    let env = ctx.data();
    let (_, mut state) = env.get_memory_and_wasi_state(&ctx, 0);

    let mut fd_map = state.fs.fd_map.write().unwrap();
    let fd_entry = wasi_try!(fd_map.get_mut(&from).ok_or(__WASI_EBADF));

    let new_fd_entry = Fd {
        // TODO: verify this is correct
        rights: fd_entry.rights_inheriting,
        ..*fd_entry
    };

    fd_map.insert(to, new_fd_entry);
    fd_map.remove(&from);
    __WASI_ESUCCESS
}

/// ### `fd_dup()`
/// Duplicates the file handle
/// Inputs:
/// - `__wasi_fd_t fd`
///   File handle to be cloned
/// Outputs:
/// - `__wasi_fd_t fd`
///   The new file handle that is a duplicate of the original
pub fn fd_dup<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    ret_fd: WasmPtr<__wasi_fd_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::fd_dup");

    let env = ctx.data();
    let (memory, state) = env.get_memory_and_wasi_state(&ctx, 0);
    let fd = wasi_try!(state.fs.clone_fd(fd));

    wasi_try_mem!(ret_fd.write(&memory, fd));

    __WASI_ESUCCESS
}

/// ### `fd_event()`
/// Creates a file handle for event notifications
pub fn fd_event<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    initial_val: u64,
    flags: __wasi_eventfdflags,
    ret_fd: WasmPtr<__wasi_fd_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::fd_event");

    let env = ctx.data();
    let (memory, state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

    let kind = Kind::EventNotifications {
        counter: Arc::new(AtomicU64::new(initial_val)),
        is_semaphore: flags & __WASI_EVENTFDFLAGS_SEMAPHORE != 0,
        wakers: Default::default(),
    };

    let inode = state.fs.create_inode_with_default_stat(
        inodes.deref_mut(),
        kind,
        false,
        "event".into(),
    );
    let rights = __WASI_RIGHT_FD_READ | __WASI_RIGHT_FD_WRITE | __WASI_RIGHT_POLL_FD_READWRITE;
    let fd = wasi_try!(state.fs.create_fd(rights, rights, 0, 0, inode));

    wasi_try_mem!(ret_fd.write(&memory, fd));

    __WASI_ESUCCESS
}

/// ### `fd_seek()`
/// Update file descriptor offset
/// Inputs:
/// - `__wasi_fd_t fd`
///     File descriptor to mutate
/// - `__wasi_filedelta_t offset`
///     Number of bytes to adjust offset by
/// - `__wasi_whence_t whence`
///     What the offset is relative to
/// Output:
/// - `__wasi_filesize_t *fd`
///     The new offset relative to the start of the file
pub fn fd_seek<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    offset: __wasi_filedelta_t,
    whence: __wasi_whence_t,
    newoffset: WasmPtr<__wasi_filesize_t, M>,
) -> Result<__wasi_errno_t, WasiError> {
    trace!("wasi::fd_seek: fd={}, offset={}", fd, offset);
    let env = ctx.data();
    let (memory, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);
    let new_offset_ref = newoffset.deref(&memory);
    let fd_entry = wasi_try_ok!(state.fs.get_fd(fd));

    if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_SEEK) {
        return Ok(__WASI_EACCES);
    }

    // TODO: handle case if fd is a dir?
    match whence {
        __WASI_WHENCE_CUR => {
            let mut fd_map = state.fs.fd_map.write().unwrap();
            let fd_entry = wasi_try_ok!(fd_map.get_mut(&fd).ok_or(__WASI_EBADF));
            fd_entry.offset = (fd_entry.offset as i64 + offset) as u64
        }
        __WASI_WHENCE_END => {
            use std::io::SeekFrom;
            let inode_idx = fd_entry.inode;
            let mut guard = inodes.arena[inode_idx].write();
            match guard.deref_mut() {
                Kind::File { ref mut handle, .. } => {
                    if let Some(handle) = handle {
                        let end =
                            wasi_try_ok!(handle.seek(SeekFrom::End(0)).map_err(map_io_err), env);

                        // TODO: handle case if fd_entry.offset uses 64 bits of a u64
                        drop(guard);
                        let mut fd_map = state.fs.fd_map.write().unwrap();
                        let fd_entry = wasi_try_ok!(fd_map.get_mut(&fd).ok_or(__WASI_EBADF));
                        fd_entry.offset = (end as i64 + offset) as u64;
                    } else {
                        return Ok(__WASI_EINVAL);
                    }
                }
                Kind::Symlink { .. } => {
                    unimplemented!("wasi::fd_seek not implemented for symlinks")
                }
                Kind::Dir { .. }
                | Kind::Root { .. }
                | Kind::Socket { .. }
                | Kind::Pipe { .. }
                | Kind::EventNotifications { .. } => {
                    // TODO: check this
                    return Ok(__WASI_EINVAL);
                }
                Kind::Buffer { .. } => {
                    // seeking buffers probably makes sense
                    // TODO: implement this
                    return Ok(__WASI_EINVAL);
                }
            }
        }
        __WASI_WHENCE_SET => {
            let mut fd_map = state.fs.fd_map.write().unwrap();
            let fd_entry = wasi_try_ok!(fd_map.get_mut(&fd).ok_or(__WASI_EBADF));
            fd_entry.offset = offset as u64
        }
        _ => return Ok(__WASI_EINVAL),
    }
    // reborrow
    let fd_entry = wasi_try_ok!(state.fs.get_fd(fd));
    wasi_try_mem_ok!(new_offset_ref.write(fd_entry.offset));

    Ok(__WASI_ESUCCESS)
}

/// ### `fd_sync()`
/// Synchronize file and metadata to disk (TODO: expand upon what this means in our system)
/// Inputs:
/// - `__wasi_fd_t fd`
///     The file descriptor to sync
/// Errors:
/// TODO: figure out which errors this should return
/// - `__WASI_EPERM`
/// - `__WASI_ENOTCAPABLE`
pub fn fd_sync(ctx: FunctionEnvMut<'_, WasiEnv>, fd: __wasi_fd_t) -> __wasi_errno_t {
    debug!("wasi::fd_sync");
    debug!("=> fd={}", fd);
    let env = ctx.data();
    let (_, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);
    let fd_entry = wasi_try!(state.fs.get_fd(fd));
    if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_SYNC) {
        return __WASI_EACCES;
    }
    let inode = fd_entry.inode;

    // TODO: implement this for more than files
    {
        let mut guard = inodes.arena[inode].write();
        match guard.deref_mut() {
            Kind::File { handle, .. } => {
                if let Some(h) = handle {
                    wasi_try!(h.sync_to_disk().map_err(fs_error_into_wasi_err));
                } else {
                    return __WASI_EINVAL;
                }
            }
            Kind::Root { .. } | Kind::Dir { .. } => return __WASI_EISDIR,
            Kind::Buffer { .. }
            | Kind::Symlink { .. }
            | Kind::Socket { .. }
            | Kind::Pipe { .. }
            | Kind::EventNotifications { .. } => return __WASI_EINVAL,
        }
    }

    __WASI_ESUCCESS
}

/// ### `fd_tell()`
/// Get the offset of the file descriptor
/// Inputs:
/// - `__wasi_fd_t fd`
///     The file descriptor to access
/// Output:
/// - `__wasi_filesize_t *offset`
///     The offset of `fd` relative to the start of the file
pub fn fd_tell<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    offset: WasmPtr<__wasi_filesize_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::fd_tell");
    let env = ctx.data();
    let (memory, mut state) = env.get_memory_and_wasi_state(&ctx, 0);
    let offset_ref = offset.deref(&memory);

    let fd_entry = wasi_try!(state.fs.get_fd(fd));

    if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_TELL) {
        return __WASI_EACCES;
    }

    wasi_try_mem!(offset_ref.write(fd_entry.offset));

    __WASI_ESUCCESS
}

/// ### `fd_write()`
/// Write data to the file descriptor
/// Inputs:
/// - `__wasi_fd_t`
///     File descriptor (opened with writing) to write to
/// - `const __wasi_ciovec_t *iovs`
///     List of vectors to read data from
/// - `u32 iovs_len`
///     Length of data in `iovs`
/// Output:
/// - `u32 *nwritten`
///     Number of bytes written
/// Errors:
///
pub fn fd_write<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    iovs: WasmPtr<__wasi_ciovec_t<M>, M>,
    iovs_len: M::Offset,
    nwritten: WasmPtr<M::Offset, M>,
) -> Result<__wasi_errno_t, WasiError> {
    trace!("wasi::fd_write: fd={}", fd);
    let env = ctx.data();
    let (memory, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);
    let iovs_arr = wasi_try_mem_ok!(iovs.slice(&memory, iovs_len));
    let nwritten_ref = nwritten.deref(&memory);

    let fd_entry = wasi_try_ok!(state.fs.get_fd(fd));
    let bytes_written = match fd {
        __WASI_STDIN_FILENO => return Ok(__WASI_EINVAL),
        __WASI_STDOUT_FILENO => {
            let mut guard = wasi_try_ok!(
                inodes
                    .stdout_mut(&state.fs.fd_map)
                    .map_err(fs_error_into_wasi_err),
                env
            );
            if let Some(ref mut stdout) = guard.deref_mut() {
                wasi_try_ok!(write_bytes(stdout, &memory, iovs_arr), env)
            } else {
                return Ok(__WASI_EBADF);
            }
        }
        __WASI_STDERR_FILENO => {
            let mut guard = wasi_try_ok!(
                inodes
                    .stderr_mut(&state.fs.fd_map)
                    .map_err(fs_error_into_wasi_err),
                env
            );
            if let Some(ref mut stderr) = guard.deref_mut() {
                wasi_try_ok!(write_bytes(stderr, &memory, iovs_arr), env)
            } else {
                return Ok(__WASI_EBADF);
            }
        }
        _ => {
            if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_WRITE) {
                return Ok(__WASI_EACCES);
            }

            let offset = fd_entry.offset as usize;
            let inode_idx = fd_entry.inode;
            let inode = &inodes.arena[inode_idx];

            let bytes_written = {
                let mut guard = inode.write();
                match guard.deref_mut() {
                    Kind::File { handle, .. } => {
                        if let Some(handle) = handle {
                            wasi_try_ok!(
                                handle
                                    .seek(std::io::SeekFrom::Start(offset as u64))
                                    .map_err(map_io_err),
                                env
                            );
                            wasi_try_ok!(write_bytes(handle, &memory, iovs_arr), env)
                        } else {
                            return Ok(__WASI_EINVAL);
                        }
                    }
                    Kind::Socket { socket } => {
                        wasi_try_ok!(socket.send(&memory, iovs_arr), env)
                    }
                    Kind::Pipe { pipe } => {
                        wasi_try_ok!(pipe.send(&memory, iovs_arr), env)
                    }
                    Kind::Dir { .. } | Kind::Root { .. } => {
                        // TODO: verify
                        return Ok(__WASI_EISDIR);
                    }
                    Kind::EventNotifications {
                        counter, wakers, ..
                    } => {
                        let mut val = 0u64.to_ne_bytes();
                        let written =
                            wasi_try_ok!(write_bytes(&mut val[..], &memory, iovs_arr));
                        if written != val.len() {
                            return Ok(__WASI_EINVAL);
                        }
                        let val = u64::from_ne_bytes(val);

                        counter.fetch_add(val, Ordering::AcqRel);
                        {
                            let mut guard = wakers.lock().unwrap();
                            while let Some(wake) = guard.pop_back() {
                                if wake.send(()).is_ok() {
                                    break;
                                }
                            }
                        }

                        written
                    }
                    Kind::Symlink { .. } => unimplemented!("Symlinks in wasi::fd_write"),
                    Kind::Buffer { buffer } => {
                        wasi_try_ok!(
                            write_bytes(&mut buffer[offset..], &memory, iovs_arr),
                            env
                        )
                    }
                }
            };

            // reborrow
            {
                let mut fd_map = state.fs.fd_map.write().unwrap();
                let fd_entry = wasi_try_ok!(fd_map.get_mut(&fd).ok_or(__WASI_EBADF));
                fd_entry.offset += bytes_written as u64;
            }
            wasi_try_ok!(state.fs.filestat_resync_size(inodes.deref(), fd), env);

            bytes_written
        }
    };

    let bytes_written: M::Offset =
        wasi_try_ok!(bytes_written.try_into().map_err(|_| __WASI_EOVERFLOW));
    wasi_try_mem_ok!(nwritten_ref.write(bytes_written));

    Ok(__WASI_ESUCCESS)
}

/// ### `fd_pipe()`
/// Creates ta pipe that feeds data between two file handles
/// Output:
/// - `__wasi_fd_t`
///     First file handle that represents one end of the pipe
/// - `__wasi_fd_t`
///     Second file handle that represents the other end of the pipe
pub fn fd_pipe<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    ro_fd1: WasmPtr<__wasi_fd_t, M>,
    ro_fd2: WasmPtr<__wasi_fd_t, M>,
) -> __wasi_errno_t {
    trace!("wasi::fd_pipe");

    let env = ctx.data();
    let (memory, state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

    let (pipe1, pipe2) = WasiPipe::new();

    let inode1 = state.fs.create_inode_with_default_stat(
        inodes.deref_mut(),
        Kind::Pipe { pipe: pipe1 },
        false,
        "pipe".into(),
    );
    let inode2 = state.fs.create_inode_with_default_stat(
        inodes.deref_mut(),
        Kind::Pipe { pipe: pipe2 },
        false,
        "pipe".into(),
    );

    let rights = super::state::all_socket_rights();
    let fd1 = wasi_try!(state.fs.create_fd(rights, rights, 0, 0, inode1));
    let fd2 = wasi_try!(state.fs.create_fd(rights, rights, 0, 0, inode2));

    wasi_try_mem!(ro_fd1.write(&memory, fd1));
    wasi_try_mem!(ro_fd2.write(&memory, fd2));

    __WASI_ESUCCESS
}

/// ### `path_create_directory()`
/// Create directory at a path
/// Inputs:
/// - `__wasi_fd_t fd`
///     The directory that the path is relative to
/// - `const char *path`
///     String containing path data
/// - `u32 path_len`
///     The length of `path`
/// Errors:
/// Required Rights:
/// - __WASI_RIGHT_PATH_CREATE_DIRECTORY
///     This right must be set on the directory that the file is created in (TODO: verify that this is true)
pub fn path_create_directory<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    path: WasmPtr<u8, M>,
    path_len: M::Offset,
) -> __wasi_errno_t {
    debug!("wasi::path_create_directory");
    let env = ctx.data();
    let (memory, state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

    let working_dir = wasi_try!(state.fs.get_fd(fd));
    {
        let guard = inodes.arena[working_dir.inode].read();
        if let Kind::Root { .. } = guard.deref() {
            return __WASI_EACCES;
        }
    }
    if !has_rights(working_dir.rights, __WASI_RIGHT_PATH_CREATE_DIRECTORY) {
        return __WASI_EACCES;
    }
    let path_string = unsafe { get_input_str!(&memory, path, path_len) };
    debug!("=> fd: {}, path: {}", fd, &path_string);

    let path = std::path::PathBuf::from(&path_string);
    let path_vec = wasi_try!(path
        .components()
        .map(|comp| {
            comp.as_os_str()
                .to_str()
                .map(|inner_str| inner_str.to_string())
                .ok_or(__WASI_EINVAL)
        })
        .collect::<Result<Vec<String>, __wasi_errno_t>>());
    if path_vec.is_empty() {
        return __WASI_EINVAL;
    }

    debug!("Looking at components {:?}", &path_vec);

    let mut cur_dir_inode = working_dir.inode;
    for comp in &path_vec {
        debug!("Creating dir {}", comp);
        let mut guard = inodes.arena[cur_dir_inode].write();
        match guard.deref_mut() {
            Kind::Dir {
                ref mut entries,
                path,
                parent,
            } => {
                match comp.borrow() {
                    ".." => {
                        if let Some(p) = parent {
                            cur_dir_inode = *p;
                            continue;
                        }
                    }
                    "." => continue,
                    _ => (),
                }
                if let Some(child) = entries.get(comp) {
                    cur_dir_inode = *child;
                } else {
                    let mut adjusted_path = path.clone();
                    drop(guard);

                    // TODO: double check this doesn't risk breaking the sandbox
                    adjusted_path.push(comp);
                    if let Ok(adjusted_path_stat) = path_filestat_get_internal(
                        &memory,
                        state,
                        inodes.deref_mut(),
                        fd,
                        0,
                        &adjusted_path.to_string_lossy(),
                    ) {
                        if adjusted_path_stat.st_filetype != __WASI_FILETYPE_DIRECTORY {
                            return __WASI_ENOTDIR;
                        }
                    } else {
                        wasi_try!(state.fs_create_dir(&adjusted_path));
                    }
                    let kind = Kind::Dir {
                        parent: Some(cur_dir_inode),
                        path: adjusted_path,
                        entries: Default::default(),
                    };
                    let new_inode = wasi_try!(state.fs.create_inode(
                        inodes.deref_mut(),
                        kind,
                        false,
                        comp.to_string()
                    ));

                    // reborrow to insert
                    {
                        let mut guard = inodes.arena[cur_dir_inode].write();
                        if let Kind::Dir {
                            ref mut entries, ..
                        } = guard.deref_mut()
                        {
                            entries.insert(comp.to_string(), new_inode);
                        }
                    }
                    cur_dir_inode = new_inode;
                }
            }
            Kind::Root { .. } => return __WASI_EACCES,
            _ => return __WASI_ENOTDIR,
        }
    }

    __WASI_ESUCCESS
}

/// ### `path_filestat_get()`
/// Access metadata about a file or directory
/// Inputs:
/// - `__wasi_fd_t fd`
///     The directory that `path` is relative to
/// - `__wasi_lookupflags_t flags`
///     Flags to control how `path` is understood
/// - `const char *path`
///     String containing the file path
/// - `u32 path_len`
///     The length of the `path` string
/// Output:
/// - `__wasi_file_stat_t *buf`
///     The location where the metadata will be stored
pub fn path_filestat_get<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    flags: __wasi_lookupflags_t,
    path: WasmPtr<u8, M>,
    path_len: M::Offset,
    buf: WasmPtr<__wasi_filestat_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::path_filestat_get (fd={})", fd);
    let env = ctx.data();
    let (memory, mut state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

    let path_string = unsafe { get_input_str!(&memory, path, path_len) };

    let stat = wasi_try!(path_filestat_get_internal(
        &memory,
        state,
        inodes.deref_mut(),
        fd,
        flags,
        &path_string
    ));

    wasi_try_mem!(buf.deref(&memory).write(stat));

    __WASI_ESUCCESS
}

/// ### `path_filestat_get()`
/// Access metadata about a file or directory
/// Inputs:
/// - `__wasi_fd_t fd`
///     The directory that `path` is relative to
/// - `__wasi_lookupflags_t flags`
///     Flags to control how `path` is understood
/// - `const char *path`
///     String containing the file path
/// - `u32 path_len`
///     The length of the `path` string
/// Output:
/// - `__wasi_file_stat_t *buf`
///     The location where the metadata will be stored
pub fn path_filestat_get_internal(
    memory: &MemoryView,
    state: &WasiState,
    inodes: &mut crate::WasiInodes,
    fd: __wasi_fd_t,
    flags: __wasi_lookupflags_t,
    path_string: &str,
) -> Result<__wasi_filestat_t, __wasi_errno_t> {
    let root_dir = state.fs.get_fd(fd)?;

    if !has_rights(root_dir.rights, __WASI_RIGHT_PATH_FILESTAT_GET) {
        return Err(__WASI_EACCES);
    }
    debug!("=> base_fd: {}, path: {}", fd, path_string);

    let file_inode = state.fs.get_inode_at_path(
        inodes,
        fd,
        path_string,
        flags & __WASI_LOOKUP_SYMLINK_FOLLOW != 0,
    )?;
    if inodes.arena[file_inode].is_preopened {
        Ok(*inodes.arena[file_inode].stat.read().unwrap().deref())
    } else {
        let guard = inodes.arena[file_inode].read();
        state.fs.get_stat_for_kind(inodes.deref(), guard.deref())
    }
}

/// ### `path_filestat_set_times()`
/// Update time metadata on a file or directory
/// Inputs:
/// - `__wasi_fd_t fd`
///     The directory relative to which the path is resolved
/// - `__wasi_lookupflags_t flags`
///     Flags to control how the path is understood
/// - `const char *path`
///     String containing the file path
/// - `u32 path_len`
///     The length of the `path` string
/// - `__wasi_timestamp_t st_atim`
///     The timestamp that the last accessed time attribute is set to
/// -  `__wasi_timestamp_t st_mtim`
///     The timestamp that the last modified time attribute is set to
/// - `__wasi_fstflags_t fst_flags`
///     A bitmask controlling which attributes are set
pub fn path_filestat_set_times<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    flags: __wasi_lookupflags_t,
    path: WasmPtr<u8, M>,
    path_len: M::Offset,
    st_atim: __wasi_timestamp_t,
    st_mtim: __wasi_timestamp_t,
    fst_flags: __wasi_fstflags_t,
) -> __wasi_errno_t {
    debug!("wasi::path_filestat_set_times");
    let env = ctx.data();
    let (memory, mut state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);
    let fd_entry = wasi_try!(state.fs.get_fd(fd));
    let fd_inode = fd_entry.inode;
    if !has_rights(fd_entry.rights, __WASI_RIGHT_PATH_FILESTAT_SET_TIMES) {
        return __WASI_EACCES;
    }
    if (fst_flags & __WASI_FILESTAT_SET_ATIM != 0 && fst_flags & __WASI_FILESTAT_SET_ATIM_NOW != 0)
        || (fst_flags & __WASI_FILESTAT_SET_MTIM != 0
            && fst_flags & __WASI_FILESTAT_SET_MTIM_NOW != 0)
    {
        return __WASI_EINVAL;
    }

    let path_string = unsafe { get_input_str!(&memory, path, path_len) };
    debug!("=> base_fd: {}, path: {}", fd, &path_string);

    let file_inode = wasi_try!(state.fs.get_inode_at_path(
        inodes.deref_mut(),
        fd,
        &path_string,
        flags & __WASI_LOOKUP_SYMLINK_FOLLOW != 0,
    ));
    let stat = {
        let guard = inodes.arena[file_inode].read();
        wasi_try!(state.fs.get_stat_for_kind(inodes.deref(), guard.deref()))
    };

    let inode = &inodes.arena[fd_inode];

    if fst_flags & __WASI_FILESTAT_SET_ATIM != 0 || fst_flags & __WASI_FILESTAT_SET_ATIM_NOW != 0 {
        let time_to_set = if fst_flags & __WASI_FILESTAT_SET_ATIM != 0 {
            st_atim
        } else {
            wasi_try!(get_current_time_in_nanos())
        };
        inode.stat.write().unwrap().st_atim = time_to_set;
    }
    if fst_flags & __WASI_FILESTAT_SET_MTIM != 0 || fst_flags & __WASI_FILESTAT_SET_MTIM_NOW != 0 {
        let time_to_set = if fst_flags & __WASI_FILESTAT_SET_MTIM != 0 {
            st_mtim
        } else {
            wasi_try!(get_current_time_in_nanos())
        };
        inode.stat.write().unwrap().st_mtim = time_to_set;
    }

    __WASI_ESUCCESS
}

/// ### `path_link()`
/// Create a hard link
/// Inputs:
/// - `__wasi_fd_t old_fd`
///     The directory relative to which the `old_path` is
/// - `__wasi_lookupflags_t old_flags`
///     Flags to control how `old_path` is understood
/// - `const char *old_path`
///     String containing the old file path
/// - `u32 old_path_len`
///     Length of the `old_path` string
/// - `__wasi_fd_t new_fd`
///     The directory relative to which the `new_path` is
/// - `const char *new_path`
///     String containing the new file path
/// - `u32 old_path_len`
///     Length of the `new_path` string
pub fn path_link<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    old_fd: __wasi_fd_t,
    old_flags: __wasi_lookupflags_t,
    old_path: WasmPtr<u8, M>,
    old_path_len: M::Offset,
    new_fd: __wasi_fd_t,
    new_path: WasmPtr<u8, M>,
    new_path_len: M::Offset,
) -> __wasi_errno_t {
    debug!("wasi::path_link");
    if old_flags & __WASI_LOOKUP_SYMLINK_FOLLOW != 0 {
        debug!("  - will follow symlinks when opening path");
    }
    let env = ctx.data();
    let (memory, mut state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);
    let old_path_str = unsafe { get_input_str!(&memory, old_path, old_path_len) };
    let new_path_str = unsafe { get_input_str!(&memory, new_path, new_path_len) };
    let source_fd = wasi_try!(state.fs.get_fd(old_fd));
    let target_fd = wasi_try!(state.fs.get_fd(new_fd));
    debug!(
        "=> source_fd: {}, source_path: {}, target_fd: {}, target_path: {}",
        old_fd, &old_path_str, new_fd, new_path_str
    );

    if !(has_rights(source_fd.rights, __WASI_RIGHT_PATH_LINK_SOURCE)
        && has_rights(target_fd.rights, __WASI_RIGHT_PATH_LINK_TARGET))
    {
        return __WASI_EACCES;
    }

    let source_inode = wasi_try!(state.fs.get_inode_at_path(
        inodes.deref_mut(),
        old_fd,
        &old_path_str,
        old_flags & __WASI_LOOKUP_SYMLINK_FOLLOW != 0,
    ));
    let target_path_arg = std::path::PathBuf::from(&new_path_str);
    let (target_parent_inode, new_entry_name) = wasi_try!(state.fs.get_parent_inode_at_path(
        inodes.deref_mut(),
        new_fd,
        &target_path_arg,
        false
    ));

    if inodes.arena[source_inode].stat.write().unwrap().st_nlink == __wasi_linkcount_t::max_value()
    {
        return __WASI_EMLINK;
    }
    {
        let mut guard = inodes.arena[target_parent_inode].write();
        match guard.deref_mut() {
            Kind::Dir { entries, .. } => {
                if entries.contains_key(&new_entry_name) {
                    return __WASI_EEXIST;
                }
                entries.insert(new_entry_name, source_inode);
            }
            Kind::Root { .. } => return __WASI_EINVAL,
            Kind::File { .. }
            | Kind::Symlink { .. }
            | Kind::Buffer { .. }
            | Kind::Socket { .. }
            | Kind::Pipe { .. }
            | Kind::EventNotifications { .. } => return __WASI_ENOTDIR,
        }
    }
    inodes.arena[source_inode].stat.write().unwrap().st_nlink += 1;

    __WASI_ESUCCESS
}

/// ### `path_open()`
/// Open file located at the given path
/// Inputs:
/// - `__wasi_fd_t dirfd`
///     The fd corresponding to the directory that the file is in
/// - `__wasi_lookupflags_t dirflags`
///     Flags specifying how the path will be resolved
/// - `char *path`
///     The path of the file or directory to open
/// - `u32 path_len`
///     The length of the `path` string
/// - `__wasi_oflags_t o_flags`
///     How the file will be opened
/// - `__wasi_rights_t fs_rights_base`
///     The rights of the created file descriptor
/// - `__wasi_rights_t fs_rightsinheriting`
///     The rights of file descriptors derived from the created file descriptor
/// - `__wasi_fdflags_t fs_flags`
///     The flags of the file descriptor
/// Output:
/// - `__wasi_fd_t* fd`
///     The new file descriptor
/// Possible Errors:
/// - `__WASI_EACCES`, `__WASI_EBADF`, `__WASI_EFAULT`, `__WASI_EFBIG?`, `__WASI_EINVAL`, `__WASI_EIO`, `__WASI_ELOOP`, `__WASI_EMFILE`, `__WASI_ENAMETOOLONG?`, `__WASI_ENFILE`, `__WASI_ENOENT`, `__WASI_ENOTDIR`, `__WASI_EROFS`, and `__WASI_ENOTCAPABLE`
pub fn path_open<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    dirfd: __wasi_fd_t,
    dirflags: __wasi_lookupflags_t,
    path: WasmPtr<u8, M>,
    path_len: M::Offset,
    o_flags: __wasi_oflags_t,
    fs_rights_base: __wasi_rights_t,
    fs_rights_inheriting: __wasi_rights_t,
    fs_flags: __wasi_fdflags_t,
    fd: WasmPtr<__wasi_fd_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::path_open");
    if dirflags & __WASI_LOOKUP_SYMLINK_FOLLOW != 0 {
        debug!("  - will follow symlinks when opening path");
    }
    let env = ctx.data();
    let (memory, mut state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);
    /* TODO: find actual upper bound on name size (also this is a path, not a name :think-fish:) */
    let path_len64: u64 = path_len.into();
    if path_len64 > 1024u64 * 1024u64 {
        return __WASI_ENAMETOOLONG;
    }

    let fd_ref = fd.deref(&memory);

    // o_flags:
    // - __WASI_O_CREAT (create if it does not exist)
    // - __WASI_O_DIRECTORY (fail if not dir)
    // - __WASI_O_EXCL (fail if file exists)
    // - __WASI_O_TRUNC (truncate size to 0)

    let working_dir = wasi_try!(state.fs.get_fd(dirfd));
    let working_dir_rights_inheriting = working_dir.rights_inheriting;

    // ASSUMPTION: open rights apply recursively
    if !has_rights(working_dir.rights, __WASI_RIGHT_PATH_OPEN) {
        return __WASI_EACCES;
    }
    let path_string = unsafe { get_input_str!(&memory, path, path_len) };

    debug!("=> fd: {}, path: {}", dirfd, &path_string);

    let path_arg = std::path::PathBuf::from(&path_string);
    let maybe_inode = state.fs.get_inode_at_path(
        inodes.deref_mut(),
        dirfd,
        &path_string,
        dirflags & __WASI_LOOKUP_SYMLINK_FOLLOW != 0,
    );

    let mut open_flags = 0;
    // TODO: traverse rights of dirs properly
    // COMMENTED OUT: WASI isn't giving appropriate rights here when opening
    //              TODO: look into this; file a bug report if this is a bug
    let adjusted_rights = /*fs_rights_base &*/ working_dir_rights_inheriting;
    let mut open_options = state.fs_new_open_options();
    let inode = if let Ok(inode) = maybe_inode {
        // Happy path, we found the file we're trying to open
        let mut guard = inodes.arena[inode].write();
        match guard.deref_mut() {
            Kind::File {
                ref mut handle,
                path,
                fd,
            } => {
                if let Some(special_fd) = fd {
                    // short circuit if we're dealing with a special file
                    assert!(handle.is_some());
                    wasi_try_mem!(fd_ref.write(*special_fd));
                    return __WASI_ESUCCESS;
                }
                if o_flags & __WASI_O_DIRECTORY != 0 {
                    return __WASI_ENOTDIR;
                }
                if o_flags & __WASI_O_EXCL != 0 {
                    return __WASI_EEXIST;
                }

                let write_permission = adjusted_rights & __WASI_RIGHT_FD_WRITE != 0;
                // append, truncate, and create all require the permission to write
                let (append_permission, truncate_permission, create_permission) =
                    if write_permission {
                        (
                            fs_flags & __WASI_FDFLAG_APPEND != 0,
                            o_flags & __WASI_O_TRUNC != 0,
                            o_flags & __WASI_O_CREAT != 0,
                        )
                    } else {
                        (false, false, false)
                    };
                let open_options = open_options
                    .read(true)
                    // TODO: ensure these rights are actually valid given parent, etc.
                    .write(write_permission)
                    .create(create_permission)
                    .append(append_permission)
                    .truncate(truncate_permission);
                open_flags |= Fd::READ;
                if adjusted_rights & __WASI_RIGHT_FD_WRITE != 0 {
                    open_flags |= Fd::WRITE;
                }
                if o_flags & __WASI_O_CREAT != 0 {
                    open_flags |= Fd::CREATE;
                }
                if o_flags & __WASI_O_TRUNC != 0 {
                    open_flags |= Fd::TRUNCATE;
                }
                *handle = Some(wasi_try!(open_options
                    .open(&path)
                    .map_err(fs_error_into_wasi_err)));
            }
            Kind::Buffer { .. } => unimplemented!("wasi::path_open for Buffer type files"),
            Kind::Dir { .. }
            | Kind::Root { .. }
            | Kind::Socket { .. }
            | Kind::Pipe { .. }
            | Kind::EventNotifications { .. } => {}
            Kind::Symlink {
                base_po_dir,
                path_to_symlink,
                relative_path,
            } => {
                // I think this should return an error (because symlinks should be resolved away by the path traversal)
                // TODO: investigate this
                unimplemented!("SYMLINKS IN PATH_OPEN");
            }
        }
        inode
    } else {
        // less-happy path, we have to try to create the file
        debug!("Maybe creating file");
        if o_flags & __WASI_O_CREAT != 0 {
            if o_flags & __WASI_O_DIRECTORY != 0 {
                return __WASI_ENOTDIR;
            }
            debug!("Creating file");
            // strip end file name

            let (parent_inode, new_entity_name) = wasi_try!(state.fs.get_parent_inode_at_path(
                inodes.deref_mut(),
                dirfd,
                &path_arg,
                dirflags & __WASI_LOOKUP_SYMLINK_FOLLOW != 0
            ));
            let new_file_host_path = {
                let guard = inodes.arena[parent_inode].read();
                match guard.deref() {
                    Kind::Dir { path, .. } => {
                        let mut new_path = path.clone();
                        new_path.push(&new_entity_name);
                        new_path
                    }
                    Kind::Root { .. } => return __WASI_EACCES,
                    _ => return __WASI_EINVAL,
                }
            };
            // once we got the data we need from the parent, we lookup the host file
            // todo: extra check that opening with write access is okay
            let handle = {
                let open_options = open_options
                    .read(true)
                    .append(fs_flags & __WASI_FDFLAG_APPEND != 0)
                    // TODO: ensure these rights are actually valid given parent, etc.
                    // write access is required for creating a file
                    .write(true)
                    .create_new(true);
                open_flags |= Fd::READ | Fd::WRITE | Fd::CREATE | Fd::TRUNCATE;

                Some(wasi_try!(open_options.open(&new_file_host_path).map_err(
                    |e| {
                        debug!("Error opening file {}", e);
                        fs_error_into_wasi_err(e)
                    }
                )))
            };

            let new_inode = {
                let kind = Kind::File {
                    handle,
                    path: new_file_host_path,
                    fd: None,
                };
                wasi_try!(state.fs.create_inode(
                    inodes.deref_mut(),
                    kind,
                    false,
                    new_entity_name.clone()
                ))
            };

            {
                let mut guard = inodes.arena[parent_inode].write();
                if let Kind::Dir {
                    ref mut entries, ..
                } = guard.deref_mut()
                {
                    entries.insert(new_entity_name, new_inode);
                }
            }

            new_inode
        } else {
            return maybe_inode.unwrap_err();
        }
    };

    // TODO: check and reduce these
    // TODO: ensure a mutable fd to root can never be opened
    let out_fd = wasi_try!(state.fs.create_fd(
        adjusted_rights,
        fs_rights_inheriting,
        fs_flags,
        open_flags,
        inode
    ));

    wasi_try_mem!(fd_ref.write(out_fd));
    debug!("wasi::path_open returning fd {}", out_fd);

    __WASI_ESUCCESS
}

/// ### `path_readlink()`
/// Read the value of a symlink
/// Inputs:
/// - `__wasi_fd_t dir_fd`
///     The base directory from which `path` is understood
/// - `const char *path`
///     Pointer to UTF-8 bytes that make up the path to the symlink
/// - `u32 path_len`
///     The number of bytes to read from `path`
/// - `u32 buf_len`
///     Space available pointed to by `buf`
/// Outputs:
/// - `char *buf`
///     Pointer to characters containing the path that the symlink points to
/// - `u32 buf_used`
///     The number of bytes written to `buf`
pub fn path_readlink<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    dir_fd: __wasi_fd_t,
    path: WasmPtr<u8, M>,
    path_len: M::Offset,
    buf: WasmPtr<u8, M>,
    buf_len: M::Offset,
    buf_used: WasmPtr<M::Offset, M>,
) -> __wasi_errno_t {
    debug!("wasi::path_readlink");
    let env = ctx.data();
    let (memory, mut state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

    let base_dir = wasi_try!(state.fs.get_fd(dir_fd));
    if !has_rights(base_dir.rights, __WASI_RIGHT_PATH_READLINK) {
        return __WASI_EACCES;
    }
    let path_str = unsafe { get_input_str!(&memory, path, path_len) };
    let inode = wasi_try!(state
        .fs
        .get_inode_at_path(inodes.deref_mut(), dir_fd, &path_str, false));

    {
        let guard = inodes.arena[inode].read();
        if let Kind::Symlink { relative_path, .. } = guard.deref() {
            let rel_path_str = relative_path.to_string_lossy();
            debug!("Result => {:?}", rel_path_str);
            let buf_len: u64 = buf_len.into();
            let bytes = rel_path_str.bytes();
            if bytes.len() as u64 >= buf_len {
                return __WASI_EOVERFLOW;
            }
            let bytes: Vec<_> = bytes.collect();

            let out =
                wasi_try_mem!(buf.slice(&memory, wasi_try!(to_offset::<M>(bytes.len()))));
            wasi_try_mem!(out.write_slice(&bytes));
            // should we null terminate this?

            let bytes_len: M::Offset =
                wasi_try!(bytes.len().try_into().map_err(|_| __WASI_EOVERFLOW));
            wasi_try_mem!(buf_used.deref(&memory).write(bytes_len));
        } else {
            return __WASI_EINVAL;
        }
    }

    __WASI_ESUCCESS
}

/// Returns __WASI_ENOTEMTPY if directory is not empty
pub fn path_remove_directory<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    path: WasmPtr<u8, M>,
    path_len: M::Offset,
) -> __wasi_errno_t {
    // TODO check if fd is a dir, ensure it's within sandbox, etc.
    debug!("wasi::path_remove_directory");
    let env = ctx.data();
    let (memory, mut state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

    let base_dir = wasi_try!(state.fs.get_fd(fd));
    let path_str = unsafe { get_input_str!(&memory, path, path_len) };

    let inode = wasi_try!(state
        .fs
        .get_inode_at_path(inodes.deref_mut(), fd, &path_str, false));
    let (parent_inode, childs_name) = wasi_try!(state.fs.get_parent_inode_at_path(
        inodes.deref_mut(),
        fd,
        std::path::Path::new(&path_str),
        false
    ));

    let host_path_to_remove = {
        let guard = inodes.arena[inode].read();
        match guard.deref() {
            Kind::Dir { entries, path, .. } => {
                if !entries.is_empty() || wasi_try!(state.fs_read_dir(path)).count() != 0 {
                    return __WASI_ENOTEMPTY;
                }
                path.clone()
            }
            Kind::Root { .. } => return __WASI_EACCES,
            _ => return __WASI_ENOTDIR,
        }
    };

    {
        let mut guard = inodes.arena[parent_inode].write();
        match guard.deref_mut() {
            Kind::Dir {
                ref mut entries, ..
            } => {
                let removed_inode = wasi_try!(entries.remove(&childs_name).ok_or(__WASI_EINVAL));
                // TODO: make this a debug assert in the future
                assert!(inode == removed_inode);
            }
            Kind::Root { .. } => return __WASI_EACCES,
            _ => unreachable!(
                "Internal logic error in wasi::path_remove_directory, parent is not a directory"
            ),
        }
    }

    if let Err(err) = state.fs_remove_dir(host_path_to_remove) {
        // reinsert to prevent FS from being in bad state
        let mut guard = inodes.arena[parent_inode].write();
        if let Kind::Dir {
            ref mut entries, ..
        } = guard.deref_mut()
        {
            entries.insert(childs_name, inode);
        }
        return err;
    }

    __WASI_ESUCCESS
}

/// ### `path_rename()`
/// Rename a file or directory
/// Inputs:
/// - `__wasi_fd_t old_fd`
///     The base directory for `old_path`
/// - `const char* old_path`
///     Pointer to UTF8 bytes, the file to be renamed
/// - `u32 old_path_len`
///     The number of bytes to read from `old_path`
/// - `__wasi_fd_t new_fd`
///     The base directory for `new_path`
/// - `const char* new_path`
///     Pointer to UTF8 bytes, the new file name
/// - `u32 new_path_len`
///     The number of bytes to read from `new_path`
pub fn path_rename<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    old_fd: __wasi_fd_t,
    old_path: WasmPtr<u8, M>,
    old_path_len: M::Offset,
    new_fd: __wasi_fd_t,
    new_path: WasmPtr<u8, M>,
    new_path_len: M::Offset,
) -> __wasi_errno_t {
    debug!(
        "wasi::path_rename: old_fd = {}, new_fd = {}",
        old_fd, new_fd
    );
    let env = ctx.data();
    let (memory, mut state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);
    let source_str = unsafe { get_input_str!(&memory, old_path, old_path_len) };
    let source_path = std::path::Path::new(&source_str);
    let target_str = unsafe { get_input_str!(&memory, new_path, new_path_len) };
    let target_path = std::path::Path::new(&target_str);
    debug!("=> rename from {} to {}", &source_str, &target_str);

    {
        let source_fd = wasi_try!(state.fs.get_fd(old_fd));
        if !has_rights(source_fd.rights, __WASI_RIGHT_PATH_RENAME_SOURCE) {
            return __WASI_EACCES;
        }
        let target_fd = wasi_try!(state.fs.get_fd(new_fd));
        if !has_rights(target_fd.rights, __WASI_RIGHT_PATH_RENAME_TARGET) {
            return __WASI_EACCES;
        }
    }

    // this is to be sure the source file is fetch from filesystem if needed
    wasi_try!(state.fs.get_inode_at_path(
        inodes.deref_mut(),
        old_fd,
        source_path.to_str().as_ref().unwrap(),
        true
    ));
    if state
        .fs
        .get_inode_at_path(
            inodes.deref_mut(),
            new_fd,
            target_path.to_str().as_ref().unwrap(),
            true,
        )
        .is_ok()
    {
        return __WASI_EEXIST;
    }
    let (source_parent_inode, source_entry_name) =
        wasi_try!(state
            .fs
            .get_parent_inode_at_path(inodes.deref_mut(), old_fd, source_path, true));
    let (target_parent_inode, target_entry_name) =
        wasi_try!(state
            .fs
            .get_parent_inode_at_path(inodes.deref_mut(), new_fd, target_path, true));

    let host_adjusted_target_path = {
        let guard = inodes.arena[target_parent_inode].read();
        match guard.deref() {
            Kind::Dir { entries, path, .. } => {
                if entries.contains_key(&target_entry_name) {
                    return __WASI_EEXIST;
                }
                let mut out_path = path.clone();
                out_path.push(std::path::Path::new(&target_entry_name));
                out_path
            }
            Kind::Root { .. } => return __WASI_ENOTCAPABLE,
            Kind::Socket { .. } | Kind::Pipe { .. } | Kind::EventNotifications { .. } => {
                return __WASI_EINVAL
            }
            Kind::Symlink { .. } | Kind::File { .. } | Kind::Buffer { .. } => {
                unreachable!("Fatal internal logic error: parent of inode is not a directory")
            }
        }
    };

    let source_entry = {
        let mut guard = inodes.arena[source_parent_inode].write();
        match guard.deref_mut() {
            Kind::Dir { entries, .. } => {
                wasi_try!(entries.remove(&source_entry_name).ok_or(__WASI_ENOENT))
            }
            Kind::Root { .. } => return __WASI_ENOTCAPABLE,
            Kind::Socket { .. } | Kind::Pipe { .. } | Kind::EventNotifications { .. } => {
                return __WASI_EINVAL
            }
            Kind::Symlink { .. } | Kind::File { .. } | Kind::Buffer { .. } => {
                unreachable!("Fatal internal logic error: parent of inode is not a directory")
            }
        }
    };

    {
        let mut guard = inodes.arena[source_entry].write();
        match guard.deref_mut() {
            Kind::File {
                handle, ref path, ..
            } => {
                // TODO: investigate why handle is not always there, it probably should be.
                // My best guess is the fact that a handle means currently open and a path
                // just means reference to host file on disk. But ideally those concepts
                // could just be unified even if there's a `Box<dyn VirtualFile>` which just
                // implements the logic of "I'm not actually a file, I'll try to be as needed".
                let result = if let Some(h) = handle {
                    drop(guard);
                    state.fs_rename(&source_path, &host_adjusted_target_path)
                } else {
                    let path_clone = path.clone();
                    drop(guard);
                    let out = state.fs_rename(&path_clone, &host_adjusted_target_path);
                    {
                        let mut guard = inodes.arena[source_entry].write();
                        if let Kind::File { ref mut path, .. } = guard.deref_mut() {
                            *path = host_adjusted_target_path;
                        } else {
                            unreachable!()
                        }
                    }
                    out
                };
                // if the above operation failed we have to revert the previous change and then fail
                if let Err(e) = result {
                    let mut guard = inodes.arena[source_parent_inode].write();
                    if let Kind::Dir { entries, .. } = guard.deref_mut() {
                        entries.insert(source_entry_name, source_entry);
                        return e;
                    }
                }
            }
            Kind::Dir { ref path, .. } => {
                let cloned_path = path.clone();
                if let Err(e) = state.fs_rename(cloned_path, &host_adjusted_target_path) {
                    return e;
                }
                {
                    drop(guard);
                    let mut guard = inodes.arena[source_entry].write();
                    if let Kind::Dir { path, .. } = guard.deref_mut() {
                        *path = host_adjusted_target_path;
                    }
                }
            }
            Kind::Buffer { .. } => {}
            Kind::Symlink { .. } => {}
            Kind::Socket { .. } => {}
            Kind::Pipe { .. } => {}
            Kind::EventNotifications { .. } => {}
            Kind::Root { .. } => unreachable!("The root can not be moved"),
        }
    }

    {
        let mut guard = inodes.arena[target_parent_inode].write();
        if let Kind::Dir { entries, .. } = guard.deref_mut() {
            let result = entries.insert(target_entry_name, source_entry);
            assert!(
                result.is_none(),
                "Fatal error: race condition on filesystem detected or internal logic error"
            );
        }
    }

    __WASI_ESUCCESS
}

/// ### `path_symlink()`
/// Create a symlink
/// Inputs:
/// - `const char *old_path`
///     Array of UTF-8 bytes representing the source path
/// - `u32 old_path_len`
///     The number of bytes to read from `old_path`
/// - `__wasi_fd_t fd`
///     The base directory from which the paths are understood
/// - `const char *new_path`
///     Array of UTF-8 bytes representing the target path
/// - `u32 new_path_len`
///     The number of bytes to read from `new_path`
pub fn path_symlink<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    old_path: WasmPtr<u8, M>,
    old_path_len: M::Offset,
    fd: __wasi_fd_t,
    new_path: WasmPtr<u8, M>,
    new_path_len: M::Offset,
) -> __wasi_errno_t {
    debug!("wasi::path_symlink");
    let env = ctx.data();
    let (memory, mut state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);
    let old_path_str = unsafe { get_input_str!(&memory, old_path, old_path_len) };
    let new_path_str = unsafe { get_input_str!(&memory, new_path, new_path_len) };
    let base_fd = wasi_try!(state.fs.get_fd(fd));
    if !has_rights(base_fd.rights, __WASI_RIGHT_PATH_SYMLINK) {
        return __WASI_EACCES;
    }

    // get the depth of the parent + 1 (UNDER INVESTIGATION HMMMMMMMM THINK FISH ^ THINK FISH)
    let old_path_path = std::path::Path::new(&old_path_str);
    let (source_inode, _) =
        wasi_try!(state
            .fs
            .get_parent_inode_at_path(inodes.deref_mut(), fd, old_path_path, true));
    let depth = wasi_try!(state
        .fs
        .path_depth_from_fd(inodes.deref(), fd, source_inode))
        - 1;

    let new_path_path = std::path::Path::new(&new_path_str);
    let (target_parent_inode, entry_name) =
        wasi_try!(state
            .fs
            .get_parent_inode_at_path(inodes.deref_mut(), fd, new_path_path, true));

    // short circuit if anything is wrong, before we create an inode
    {
        let guard = inodes.arena[target_parent_inode].read();
        match guard.deref() {
            Kind::Dir { entries, .. } => {
                if entries.contains_key(&entry_name) {
                    return __WASI_EEXIST;
                }
            }
            Kind::Root { .. } => return __WASI_ENOTCAPABLE,
            Kind::Socket { .. } | Kind::Pipe { .. } | Kind::EventNotifications { .. } => {
                return __WASI_EINVAL
            }
            Kind::File { .. } | Kind::Symlink { .. } | Kind::Buffer { .. } => {
                unreachable!("get_parent_inode_at_path returned something other than a Dir or Root")
            }
        }
    }

    let mut source_path = std::path::Path::new(&old_path_str);
    let mut relative_path = std::path::PathBuf::new();
    for _ in 0..depth {
        relative_path.push("..");
    }
    relative_path.push(source_path);
    debug!(
        "Symlinking {} to {}",
        new_path_str,
        relative_path.to_string_lossy()
    );

    let kind = Kind::Symlink {
        base_po_dir: fd,
        path_to_symlink: std::path::PathBuf::from(new_path_str),
        relative_path,
    };
    let new_inode = state.fs.create_inode_with_default_stat(
        inodes.deref_mut(),
        kind,
        false,
        entry_name.clone().into(),
    );

    {
        let mut guard = inodes.arena[target_parent_inode].write();
        if let Kind::Dir {
            ref mut entries, ..
        } = guard.deref_mut()
        {
            entries.insert(entry_name, new_inode);
        }
    }

    __WASI_ESUCCESS
}

/// ### `path_unlink_file()`
/// Unlink a file, deleting if the number of hardlinks is 1
/// Inputs:
/// - `__wasi_fd_t fd`
///     The base file descriptor from which the path is understood
/// - `const char *path`
///     Array of UTF-8 bytes representing the path
/// - `u32 path_len`
///     The number of bytes in the `path` array
pub fn path_unlink_file<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: __wasi_fd_t,
    path: WasmPtr<u8, M>,
    path_len: M::Offset,
) -> __wasi_errno_t {
    debug!("wasi::path_unlink_file");
    let env = ctx.data();
    let (memory, mut state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

    let base_dir = wasi_try!(state.fs.get_fd(fd));
    if !has_rights(base_dir.rights, __WASI_RIGHT_PATH_UNLINK_FILE) {
        return __WASI_EACCES;
    }
    let path_str = unsafe { get_input_str!(&memory, path, path_len) };
    debug!("Requested file: {}", path_str);

    let inode = wasi_try!(state
        .fs
        .get_inode_at_path(inodes.deref_mut(), fd, &path_str, false));
    let (parent_inode, childs_name) = wasi_try!(state.fs.get_parent_inode_at_path(
        inodes.deref_mut(),
        fd,
        std::path::Path::new(&path_str),
        false
    ));

    let removed_inode = {
        let mut guard = inodes.arena[parent_inode].write();
        match guard.deref_mut() {
            Kind::Dir {
                ref mut entries, ..
            } => {
                let removed_inode = wasi_try!(entries.remove(&childs_name).ok_or(__WASI_EINVAL));
                // TODO: make this a debug assert in the future
                assert!(inode == removed_inode);
                debug_assert!(inodes.arena[inode].stat.read().unwrap().st_nlink > 0);
                removed_inode
            }
            Kind::Root { .. } => return __WASI_EACCES,
            _ => unreachable!(
                "Internal logic error in wasi::path_unlink_file, parent is not a directory"
            ),
        }
    };

    let st_nlink = {
        let mut guard = inodes.arena[removed_inode].stat.write().unwrap();
        guard.st_nlink -= 1;
        guard.st_nlink
    };
    if st_nlink == 0 {
        {
            let mut guard = inodes.arena[removed_inode].write();
            match guard.deref_mut() {
                Kind::File { handle, path, .. } => {
                    if let Some(h) = handle {
                        wasi_try!(h.unlink().map_err(fs_error_into_wasi_err));
                    } else {
                        // File is closed
                        // problem with the abstraction, we can't call unlink because there's no handle
                        // drop mutable borrow on `path`
                        let path = path.clone();
                        wasi_try!(state.fs_remove_file(path));
                    }
                }
                Kind::Dir { .. } | Kind::Root { .. } => return __WASI_EISDIR,
                Kind::Symlink { .. } => {
                    // TODO: actually delete real symlinks and do nothing for virtual symlinks
                }
                _ => unimplemented!("wasi::path_unlink_file for Buffer"),
            }
        }
        // TODO: test this on Windows and actually make it portable
        // make the file an orphan fd if the fd is still open
        let fd_is_orphaned = {
            let guard = inodes.arena[removed_inode].read();
            if let Kind::File { handle, .. } = guard.deref() {
                handle.is_some()
            } else {
                false
            }
        };
        let removed_inode_val = unsafe { state.fs.remove_inode(inodes.deref_mut(), removed_inode) };
        assert!(
            removed_inode_val.is_some(),
            "Inode could not be removed because it doesn't exist"
        );

        if fd_is_orphaned {
            inodes
                .orphan_fds
                .insert(removed_inode, removed_inode_val.unwrap());
        }
    }

    __WASI_ESUCCESS
}

/// ### `poll_oneoff()`
/// Concurrently poll for a set of events
/// Inputs:
/// - `const __wasi_subscription_t *in`
///     The events to subscribe to
/// - `__wasi_event_t *out`
///     The events that have occured
/// - `u32 nsubscriptions`
///     The number of subscriptions and the number of events
/// Output:
/// - `u32 nevents`
///     The number of events seen
pub fn poll_oneoff<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    in_: WasmPtr<__wasi_subscription_t, M>,
    out_: WasmPtr<__wasi_event_t, M>,
    nsubscriptions: M::Offset,
    nevents: WasmPtr<M::Offset, M>,
) -> Result<__wasi_errno_t, WasiError> {
    trace!("wasi::poll_oneoff");
    trace!("  => nsubscriptions = {}", nsubscriptions);
    
    let mut events_seen: u32 = 0;
    
    let mut clock_subs = vec![];
    let mut in_events = vec![];
    let mut time_to_sleep = Duration::from_millis(5);

    let inodes = ctx.data().state.inodes.clone();
    let inodes = inodes.read().unwrap();
    let mut fd_guards = vec![];

    #[allow(clippy::significant_drop_in_scrutinee)]
    let fds = {
        let env = ctx.data();
        let memory = env.memory_view(&ctx);
        let state = env.state.deref();
        
        let subscription_array = wasi_try_mem_ok!(in_.slice(&memory, nsubscriptions));
        for sub in subscription_array.iter() {
            let s: WasiSubscription = wasi_try_ok!(wasi_try_mem_ok!(sub.read()).try_into());
            let mut peb = PollEventBuilder::new();

            let fd = match s.event_type {
                EventType::Read(__wasi_subscription_fs_readwrite_t { fd }) => {
                    match fd {
                        __WASI_STDIN_FILENO | __WASI_STDOUT_FILENO | __WASI_STDERR_FILENO => (),
                        _ => {
                            let fd_entry = wasi_try_ok!(state.fs.get_fd(fd), env);
                            if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_READ) {
                                return Ok(__WASI_EACCES);
                            }
                        }
                    }
                    in_events.push(peb.add(PollEvent::PollIn).build());
                    Some(fd)
                }
                EventType::Write(__wasi_subscription_fs_readwrite_t { fd }) => {
                    match fd {
                        __WASI_STDIN_FILENO | __WASI_STDOUT_FILENO | __WASI_STDERR_FILENO => (),
                        _ => {
                            let fd_entry = wasi_try_ok!(state.fs.get_fd(fd), env);
                            if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_WRITE) {
                                return Ok(__WASI_EACCES);
                            }
                        }
                    }
                    in_events.push(peb.add(PollEvent::PollOut).build());
                    Some(fd)
                }
                EventType::Clock(clock_info) => {
                    if clock_info.clock_id == __WASI_CLOCK_REALTIME
                        || clock_info.clock_id == __WASI_CLOCK_MONOTONIC
                    {
                        // this is a hack
                        // TODO: do this properly
                        time_to_sleep = Duration::from_nanos(clock_info.timeout);
                        clock_subs.push((clock_info, s.user_data));
                        None
                    } else {
                        unimplemented!("Polling not implemented for clocks yet");
                    }
                }
            };

            if let Some(fd) = fd {
                let wasi_file_ref = match fd {
                    __WASI_STDERR_FILENO => {
                        wasi_try_ok!(
                            inodes
                                .stderr(&state.fs.fd_map)
                                .map_err(fs_error_into_wasi_err),
                            env
                        )
                    }
                    __WASI_STDIN_FILENO => {
                        wasi_try_ok!(
                            inodes
                                .stdin(&state.fs.fd_map)
                                .map_err(fs_error_into_wasi_err),
                            env
                        )
                    }
                    __WASI_STDOUT_FILENO => {
                        wasi_try_ok!(
                            inodes
                                .stdout(&state.fs.fd_map)
                                .map_err(fs_error_into_wasi_err),
                            env
                        )
                    }
                    _ => {
                        let fd_entry = wasi_try_ok!(state.fs.get_fd(fd), env);
                        let inode = fd_entry.inode;
                        if !has_rights(fd_entry.rights, __WASI_RIGHT_POLL_FD_READWRITE) {
                            return Ok(__WASI_EACCES);
                        }

                        {
                            let guard = inodes.arena[inode].read();
                            match guard.deref() {
                                Kind::File { handle, .. } => {
                                    if let Some(h) = handle {
                                        crate::state::InodeValFileReadGuard { guard }
                                    } else {
                                        return Ok(__WASI_EBADF);
                                    }
                                }
                                Kind::Socket { .. }
                                | Kind::Pipe { .. }
                                | Kind::EventNotifications { .. } => {
                                    return Ok(__WASI_EBADF);
                                }
                                Kind::Dir { .. }
                                | Kind::Root { .. }
                                | Kind::Buffer { .. }
                                | Kind::Symlink { .. } => {
                                    unimplemented!("polling read on non-files not yet supported")
                                }
                            }
                        }
                    }
                };
                fd_guards.push(wasi_file_ref);
            }
        }
    
        let mut f = vec![];
        for fd in fd_guards.iter() {
            f.push(wasi_try_ok!(fd.as_ref().ok_or(__WASI_EBADF)).deref());
        }
        f
    };

    let mut seen_events = vec![Default::default(); in_events.len()];

    let start = platform_clock_time_get(__WASI_CLOCK_MONOTONIC, 1_000_000).unwrap() as u128;
    let mut triggered = 0;
    while triggered == 0 {
        let now = platform_clock_time_get(__WASI_CLOCK_MONOTONIC, 1_000_000).unwrap() as u128;
        let delta = match now.checked_sub(start) {
            Some(a) => Duration::from_nanos(a as u64),
            None => Duration::ZERO,
        };
        match poll(
            fds.as_slice(),
            in_events.as_slice(),
            seen_events.as_mut_slice(),
            Duration::from_millis(1),
        ) {
            Ok(0) => {
                ctx.data().clone().yield_now_with_signals(&mut ctx)?;
            }
            Ok(a) => {
                triggered = a;
            }
            Err(FsError::WouldBlock) => {
                ctx.data().clone().sleep(&mut ctx, Duration::from_millis(1))?;
            }
            Err(err) => {
                return Ok(fs_error_into_wasi_err(err));
            }
        };
        if delta > time_to_sleep {
            break;
        }
    }

    for (i, seen_event) in seen_events.into_iter().enumerate() {
        let mut env = ctx.data();
        let mut flags = 0;
        let mut error = __WASI_EAGAIN;
        let mut bytes_available = 0;
        let event_iter = iterate_poll_events(seen_event);
        for event in event_iter {
            match event {
                PollEvent::PollError => error = __WASI_EIO,
                PollEvent::PollHangUp => flags = __WASI_EVENT_FD_READWRITE_HANGUP,
                PollEvent::PollInvalid => error = __WASI_EINVAL,
                PollEvent::PollIn => {
                    bytes_available = wasi_try_ok!(
                        fds[i]
                            .bytes_available_read()
                            .map_err(fs_error_into_wasi_err),
                        env
                    )
                    .unwrap_or(0usize);
                    error = __WASI_ESUCCESS;
                }
                PollEvent::PollOut => {
                    bytes_available = wasi_try_ok!(
                        fds[i]
                            .bytes_available_write()
                            .map_err(fs_error_into_wasi_err),
                        env
                    )
                    .unwrap_or(0usize);
                    error = __WASI_ESUCCESS;
                }
            }
        }
        let memory = env.memory_view(&ctx);
        let subscription_array = wasi_try_mem_ok!(in_.slice(&memory, nsubscriptions));
        let event_array = wasi_try_mem_ok!(out_.slice(&memory, nsubscriptions));

        let event = __wasi_event_t {
            userdata: wasi_try_mem_ok!(subscription_array.index(i as u64).read()).userdata,
            error,
            type_: wasi_try_mem_ok!(subscription_array.index(i as u64).read()).type_,
            u: unsafe {
                __wasi_event_u {
                    fd_readwrite: __wasi_event_fd_readwrite_t {
                        nbytes: bytes_available as u64,
                        flags,
                    },
                }
            },
        };
        wasi_try_mem_ok!(event_array.index(events_seen as u64).write(event));
        events_seen += 1;
    }
    if triggered == 0 {
        for (clock_info, userdata) in clock_subs {
            let event = __wasi_event_t {
                userdata,
                error: __WASI_ESUCCESS,
                type_: __WASI_EVENTTYPE_CLOCK,
                u: unsafe {
                    __wasi_event_u {
                        fd_readwrite: __wasi_event_fd_readwrite_t {
                            nbytes: 0,
                            flags: 0,
                        },
                    }
                },
            };
            
            let mut env = ctx.data();
            let memory = env.memory_view(&ctx);
            let event_array = wasi_try_mem_ok!(out_.slice(&memory, nsubscriptions));

            wasi_try_mem_ok!(event_array.index(events_seen as u64).write(event));
            events_seen += 1;
        }
    }
    let events_seen: M::Offset = wasi_try_ok!(events_seen.try_into().map_err(|_| __WASI_EOVERFLOW));

    let mut env = ctx.data();
    let memory = env.memory_view(&ctx);
    let out_ptr = nevents.deref(&memory);
    wasi_try_mem_ok!(out_ptr.write(events_seen));
    Ok(__WASI_ESUCCESS)
}

/// ### `proc_exit()`
/// Terminate the process normally. An exit code of 0 indicates successful
/// termination of the program. The meanings of other values is dependent on
/// the environment.
/// Inputs:
/// - `__wasi_exitcode_t`
///   Exit code to return to the operating system
pub fn proc_exit(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    code: __wasi_exitcode_t,
) -> Result<(), WasiError> {
    debug!("wasi::proc_exit, {}", code);
    Err(WasiError::Exit(code))
}

/// ### `thread_signal()`
/// Send a signal to a particular thread in the current process.
/// Note: This is similar to `signal` in POSIX.
/// Inputs:
/// - `__wasi_signal_t`
///   Signal to be raised for this process
pub fn thread_signal(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    tid: __wasi_tid_t,
    sig: __wasi_signal_t
) -> Result<__wasi_errno_t, WasiError> {
    debug!("wasi::thread_signal(tid={}, sig={})", tid, sig);
    {
        let guard = ctx.data().state().threading.read().unwrap();
        let tid = WasiThreadId(tid);
        guard.signal(&tid, sig);
    }
    
    let env = ctx.data();
    env.clone().yield_now_with_signals(&mut ctx)?;

    Ok(__WASI_ESUCCESS)
}

/// ### `proc_raise()`
/// Send a signal to the process of the calling thread.
/// Note: This is similar to `raise` in POSIX.
/// Inputs:
/// - `__wasi_signal_t`
///   Signal to be raised for this process
pub fn proc_raise(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    sig: __wasi_signal_t
) -> Result<__wasi_errno_t, WasiError> {
    debug!("wasi::proc_raise (sig={})", sig);
    {
        let guard = ctx.data().state().threading.read().unwrap();
        guard.signal_all(sig);
    }

    let env = ctx.data();
    env.clone().yield_now_with_signals(&mut ctx)?;

    Ok(__WASI_ESUCCESS)
}

/// ### `sched_yield()`
/// Yields execution of the thread
pub fn sched_yield(
    mut ctx: FunctionEnvMut<'_, WasiEnv>
) -> Result<__wasi_errno_t, WasiError> {
    trace!("wasi::sched_yield");
    let env = ctx.data();
    env.clone().yield_now_with_signals(&mut ctx)?;
    Ok(__WASI_ESUCCESS)
}

fn get_stack_base(
    mut ctx: &mut FunctionEnvMut<'_, WasiEnv>,
) -> u64 {
    ctx.data().stack_base
}

fn get_stack_start(
    mut ctx: &mut FunctionEnvMut<'_, WasiEnv>,
) -> u64 {
    ctx.data().stack_start
}

fn get_memory_stack_pointer(
    ctx: &mut FunctionEnvMut<'_, WasiEnv>,
) -> Result<u64, String>
{
    // Get the current value of the stack pointer (which we will use
    // to save all of the stack)
    let stack_base = get_stack_base(ctx);
    let stack_pointer = if let Some(stack_pointer) = ctx.data().inner().stack_pointer.clone() {
        match stack_pointer.get(ctx) {
            Value::I32(a) => a as u64,
            Value::I64(a) => a as u64,
            _ => stack_base
        }
    } else {
        return Err(format!("failed to save stack: not exported __stack_pointer global"));
    };
    Ok(stack_pointer)
}

fn get_memory_stack_offset(
    ctx: &mut FunctionEnvMut<'_, WasiEnv>,
) -> Result<u64, String>
{
    let stack_base = get_stack_base(ctx);
    let stack_pointer = get_memory_stack_pointer(ctx)?;
    Ok(stack_base - stack_pointer)
}

fn set_memory_stack_offset(
    ctx: &mut FunctionEnvMut<'_, WasiEnv>,
    offset: u64,
) -> Result<(), String>
{
    // Sets the stack pointer
    let stack_base = get_stack_base(ctx);
    let stack_pointer = stack_base - offset;
    if let Some(stack_pointer_ptr) = ctx.data().inner().stack_pointer.clone() {
        match stack_pointer_ptr.get(ctx) {
            Value::I32(_) => {
                stack_pointer_ptr.set(ctx, Value::I32(stack_pointer as i32));
            },
            Value::I64(_) => {
                stack_pointer_ptr.set(ctx, Value::I64(stack_pointer as i64));
            },
            _ => {
                return Err(format!("failed to save stack: __stack_pointer global is of an unknown type"));
            }
        }
    } else {
        return Err(format!("failed to save stack: not exported __stack_pointer global"));
    }
    Ok(())
}

#[allow(dead_code)]
fn get_memory_stack<M: MemorySize>(
    ctx: &mut FunctionEnvMut<'_, WasiEnv>,
) -> Result<Vec<u8>, String> {
    // Get the current value of the stack pointer (which we will use
    // to save all of the stack)
    let stack_base = get_stack_base(ctx);
    let stack_pointer = if let Some(stack_pointer) = ctx.data().inner().stack_pointer.clone() {
        match stack_pointer.get(ctx) {
            Value::I32(a) => a as u64,
            Value::I64(a) => a as u64,
            _ => stack_base
        }
    } else {
        return Err(format!("failed to save stack: not exported __stack_pointer global"));
    };
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let stack_offset = env.stack_base - stack_pointer;

    // Read the memory stack into a vector
    let memory_stack_ptr = WasmPtr::<u8, M>::new(stack_pointer.try_into().map_err(|_| {
        format!("failed to save stack: stack pointer overflow")
    })?);
    memory_stack_ptr.slice(&memory, stack_offset.try_into().map_err(|_| {
        format!("failed to save stack: stack pointer overflow")
    })?)
        .and_then(|memory_stack| {
            memory_stack.read_to_vec()
        })
        .map_err(|err| {
            format!("failed to read stack: {}", err)
        })
}

#[allow(dead_code)]
fn set_memory_stack<M: MemorySize>(
    mut ctx: &mut FunctionEnvMut<'_, WasiEnv>,
    stack: Vec<u8>
) -> Result<(), String> {
    // First we restore the memory stack
    let stack_base = get_stack_base(ctx);
    let stack_offset = stack.len() as u64;
    let stack_pointer = stack_base - stack_offset;
    let stack_ptr = WasmPtr::<u8, M>::new(stack_pointer.try_into().map_err(|_| {
        format!("failed to restore stack: stack pointer overflow")
    })?);

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    stack_ptr.slice(&memory, stack_offset.try_into().map_err(|_| {
        format!("failed to restore stack: stack pointer overflow")
    })?)
        .and_then(|memory_stack| {
            memory_stack.write_slice(&stack[..])
        })
        .map_err(|err| {
            format!("failed to write stack: {}", err)
        })?;

    // Set the stack pointer itself and return
    set_memory_stack_offset(ctx, stack_offset)?;
    Ok(())
}

#[must_use = "you must return the result immediately so the stack can unwind"]
fn unwind<M: MemorySize, F>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    callback: F,
) -> Result<__wasi_errno_t, WasiError>
where F: FnOnce(FunctionEnvMut<'_, WasiEnv>, Vec<u8>, Vec<u8>) -> OnCalledAction + Send + Sync + 'static,
{
    // Get the current stack pointer (this will be used to determine the
    // upper limit of stack space remaining to unwind into)
    let memory_stack = match get_memory_stack::<M>(&mut ctx) {
        Ok(a) => a,
        Err(err) => {
            warn!("unable to get the memory stack - {}", err);
            return Err(WasiError::Exit(__WASI_EFAULT as __wasi_exitcode_t));
        }
    };
    
    // Perform a check to see if we have enough room
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    
    // Write the addresses to the start of the stack space
    let unwind_pointer: u64 = wasi_try_ok!(env.stack_start.try_into().map_err(|_| __WASI_EOVERFLOW));
    let unwind_data_start = unwind_pointer + (std::mem::size_of::<__wasi_asyncify_t<M::Offset>>() as u64);
    let unwind_data = __wasi_asyncify_t::<M::Offset> {
        start: wasi_try_ok!(unwind_data_start.try_into().map_err(|_| __WASI_EOVERFLOW)),
        end: wasi_try_ok!(env.stack_base.try_into().map_err(|_| __WASI_EOVERFLOW)),
    };
    let unwind_data_ptr: WasmPtr<__wasi_asyncify_t<M::Offset>, M> = WasmPtr::new(
        wasi_try_ok!(unwind_pointer.try_into().map_err(|_| __WASI_EOVERFLOW))
    );
    wasi_try_mem_ok!(
        unwind_data_ptr.write(&memory, unwind_data)
    );
    
    // Invoke the callback that will prepare to unwind
    // We need to start unwinding the stack
    let asyncify_data = wasi_try_ok!(unwind_pointer.try_into().map_err(|_| __WASI_EOVERFLOW));
    if let Some(asyncify_start_unwind) = env.inner().asyncify_start_unwind.clone() {
        asyncify_start_unwind.call(&mut ctx, asyncify_data);
    } else {
        warn!("failed to unwind the stack because the asyncify_start_rewind export is missing");
        return Err(WasiError::Exit(128));
    }

    // Set callback that will be invoked when this process finishes
    let env = ctx.data();
    let unwind_stack_begin: u64 = unwind_data.start.into();
    let unwind_space = env.stack_base - env.stack_start;
    let func = ctx.as_ref();
    trace!("-- unwinding -- (memory_stack_size={} unwind_space={})", memory_stack.len(), unwind_space);
    ctx.as_store_mut().on_called(move |mut store| {
        let mut ctx = func.into_mut(&mut store);
        let env = ctx.data();
        let memory = env.memory_view(&ctx);

        let unwind_data_ptr: WasmPtr<__wasi_asyncify_t<M::Offset>, M> = WasmPtr::new(
            unwind_pointer.try_into().map_err(|_| __WASI_EOVERFLOW).unwrap()
        );
        let unwind_data_result = unwind_data_ptr.read(&memory).unwrap();
        let unwind_stack_finish: u64 = unwind_data_result.start.into();
        let unwind_size = unwind_stack_finish - unwind_stack_begin;
        trace!("-- unwound - (memory_stack_size={} unwind_size={})", memory_stack.len(), unwind_size);
        
        // Read the memory stack into a vector
        let unwind_stack_ptr = WasmPtr::<u8, M>::new(unwind_stack_begin.try_into().map_err(|_| {
            format!("failed to save stack: stack pointer overflow")
        })?);
        let unwind_stack = unwind_stack_ptr.slice(&memory, unwind_size.try_into().map_err(|_| {
            format!("failed to save stack: stack pointer overflow")
        })?)
            .and_then(|memory_stack| {
                memory_stack.read_to_vec()
            })
            .map_err(|err| {
                format!("failed to read stack: {}", err)
            })?;

        // Notify asyncify that we are no longer unwinding
        if let Some(asyncify_stop_unwind) = env.inner().asyncify_stop_unwind.clone() {
            asyncify_stop_unwind.call(&mut ctx);
        } else {
            warn!("failed to unwind the stack because the asyncify_start_rewind export is missing");
            return Ok(OnCalledAction::Finish);
        }

        Ok(
            callback(ctx, memory_stack, unwind_stack)
        )
    });

    // We need to exit the function so that it can unwind and then invoke the callback
    Ok(__WASI_ESUCCESS)
}

#[must_use = "the action must be passed to the call loop"]
fn rewind<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    memory_stack: Vec<u8>,
    rewind_stack: Vec<u8>,
) -> __wasi_errno_t
{
    trace!("-- rewinding -- (memory_stack_size={}, rewind_size={})", memory_stack.len(), rewind_stack.len());

    // Perform a check to see if we have enough room
    let env = ctx.data();
    let memory = env.memory_view(&ctx);

    // Store the memory stack so that it can be restored later
    super::REWIND.with(|cell| cell.replace(Some(memory_stack)));

    // Write the addresses to the start of the stack space
    let rewind_pointer: u64 = wasi_try!(env.stack_start.try_into().map_err(|_| __WASI_EOVERFLOW));
    let rewind_data_start = rewind_pointer + (std::mem::size_of::<__wasi_asyncify_t<M::Offset>>() as u64);
    let rewind_data_end = rewind_data_start + (rewind_stack.len() as u64);
    if rewind_data_end > env.stack_base {
        warn!("attempting to rewind a stack bigger than the allocated stack space ({} > {})", rewind_data_end, env.stack_base);
        return __WASI_EOVERFLOW;
    }
    let rewind_data = __wasi_asyncify_t::<M::Offset> {
        start: wasi_try!(rewind_data_end.try_into().map_err(|_| __WASI_EOVERFLOW)),
        end: wasi_try!(env.stack_base.try_into().map_err(|_| __WASI_EOVERFLOW)),
    };
    let rewind_data_ptr: WasmPtr<__wasi_asyncify_t<M::Offset>, M> = WasmPtr::new(
        wasi_try!(rewind_pointer.try_into().map_err(|_| __WASI_EOVERFLOW))
    );
    wasi_try_mem!(
        rewind_data_ptr.write(&memory, rewind_data)
    );

    // Copy the data to the address
    let rewind_stack_ptr = WasmPtr::<u8, M>::new(wasi_try!(rewind_data_start.try_into().map_err(|_| __WASI_EOVERFLOW)));
    wasi_try_mem!(rewind_stack_ptr.slice(&memory, wasi_try!(rewind_stack.len().try_into().map_err(|_| __WASI_EOVERFLOW)))
        .and_then(|stack| {
            stack.write_slice(&rewind_stack[..])
        }));
    
    // Invoke the callback that will prepare to rewind
    let asyncify_data = wasi_try!(rewind_pointer.try_into().map_err(|_| __WASI_EOVERFLOW));
    if let Some(asyncify_start_rewind) = env.inner().asyncify_start_rewind.clone() {
        asyncify_start_rewind.call(&mut ctx, asyncify_data);
    } else {
        warn!("failed to rewind the stack because the asyncify_start_rewind export is missing");
        return __WASI_EFAULT;
    }
    
    __WASI_ESUCCESS
}

fn handle_rewind<M: MemorySize>(
    ctx: &mut FunctionEnvMut<'_, WasiEnv>,
) -> bool {
    // If the stack has been restored
    if let Some(memory_stack) = super::REWIND.with(|cell| cell.borrow_mut().take())
    {
        // Notify asyncify that we are no longer rewinding
        let env = ctx.data();
        if let Some(asyncify_stop_rewind) = env.inner().asyncify_stop_rewind.clone() {
            asyncify_stop_rewind.call(ctx);
        }

        // Restore the memory stack
        set_memory_stack::<M>(ctx, memory_stack);
        true
    } else {
        false
    }
}

/// ### `stack_checkpoint()`
/// Creates a snapshot of the current stack which allows it to be restored
/// later using its stack hash.
pub fn stack_checkpoint<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    snapshot_ptr: WasmPtr<__wasi_stack_snaphost_t, M>,
    ret_val: WasmPtr<__wasi_longsize_t, M>,
) -> Result<__wasi_errno_t, WasiError>
{
    // If we were just restored then we need to return the value instead
    if handle_rewind::<M>(&mut ctx) {
        let env = ctx.data();
        let memory = env.memory_view(&ctx);
        let ret_val = wasi_try_mem_ok!(
            ret_val.read(&memory)
        );
        trace!("wasi::stack_checkpoint - restored - (ret={})", ret_val);
        return Ok(__WASI_ESUCCESS);
    }
    trace!("wasi::stack_checkpoint - capturing");

    // Set the return value that we will give back to
    // indicate we are a normal function call
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    wasi_try_mem_ok!(
        ret_val.write(&memory, 0)
    );

    // Pass some offsets to the unwind function
    let ret_offset = ret_val.offset();
    let snapshot_offset = snapshot_ptr.offset();
    let secret = env.state().secret.clone();

    // Perform the unwind action
    unwind::<M, _>(ctx, move |mut ctx, memory_stack, rewind_stack|
    {
        // We compute the hash again for two reasons... integrity so if there
        // is a long jump that goes to the wrong place it will fail gracefully.
        // and security so that the stack can not be used to attempt to break
        // out of the sandbox
        let hash = {
            use sha2::{Sha256, Digest};
            let mut hasher = Sha256::new();
            hasher.update(&secret[..]);
            hasher.update(&memory_stack[..]);
            hasher.update(&rewind_stack[..]);
            let hash: [u8; 16] = hasher.finalize()[..16].try_into().unwrap();
            u128::from_le_bytes(hash)
        };

        // Build the upper stack(s)
        let mut upper_rewind_stack = [0u8; 256];
        let upper_stack_clip = rewind_stack.len().min(256);
        upper_rewind_stack[..upper_stack_clip]
            .copy_from_slice(&rewind_stack[..upper_stack_clip]);
        
        let mut upper_memory_stack = [0u8; 256];
        let upper_stack_clip = memory_stack.len().min(256);
        upper_memory_stack[..upper_stack_clip]
            .copy_from_slice(&memory_stack[..upper_stack_clip]);

        // Build a stack snapshot
        let snapshot = __wasi_stack_snaphost_t {
            memory_offset: memory_stack.len() as u32,
            host_offset: rewind_stack.len() as u32,
            user: ret_offset.into(),
            upper_rewind_stack,
            upper_memory_stack,
            hash
        };

        // Save the stack snapshot
        let env = ctx.data();
        let memory = env.memory_view(&ctx);
        let snapshot_ptr: WasmPtr<__wasi_stack_snaphost_t, M> = WasmPtr::new(snapshot_offset);
        if let Err(err) = snapshot_ptr.write(&memory, snapshot) {
            warn!("failed checkpoint - could not save stack snapshot - {}", err);
            return OnCalledAction::Trap(Box::new(WasiError::Exit(__WASI_EFAULT as u32)));
        }

        // Rewind the stack and carry on
        match rewind::<M>(ctx, memory_stack, rewind_stack) {
            __WASI_ESUCCESS => OnCalledAction::InvokeAgain,
            err => {
                warn!("failed checkpoint - could not rewind the stack - errno={}", err);
                OnCalledAction::Trap(Box::new(WasiError::Exit(err as u32)))
            }
        }
    })
}

/// ### `stack_restore()`
/// Restores the current stack to a previous stack described by its
/// stack hash.
///
/// ## Parameters
///
/// * `snapshot_ptr` - Contains a previously made snapshot
pub fn stack_restore<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    snapshot_ptr: WasmPtr<__wasi_stack_snaphost_t, M>,
    mut val: __wasi_longsize_t,
) -> Result<(), WasiError> {
    trace!("wasi::stack_restore (with_ret={})", val);

    // Read the snapshot from the stack
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let snapshot = match snapshot_ptr.read(&memory) {
        Ok(a) => a,
        Err(err) => {
            warn!("wasi::stack_restore - failed to read stack snapshot - {}", err);
            return Err(WasiError::Exit(128));
        }
    };

    // Perform the unwind action
    unwind::<M, _>(ctx, move |mut ctx, memory_stack, rewind_stack|
    {
        let env = ctx.data();
        let memory = env.memory_view(&ctx);
        let secret = env.state().secret.clone();

        // Build a new rewind stack using the current stack and the data that was supplied
        let rewind_stack = {
            let upper = snapshot.upper_rewind_stack.len()
                .min(snapshot.host_offset as usize);
            let mut s = snapshot.upper_rewind_stack[..upper].to_vec();
            if snapshot.host_offset as usize > upper {
                let remainder = snapshot.host_offset as usize - upper;
                if remainder >= rewind_stack.len() {
                    warn!("wasi::stack_restore - current rewind stack is too small to restore the snapshot");
                    return OnCalledAction::Trap(Box::new(WasiError::Exit(__WASI_EFAULT as u32)));
                }
                let lower = rewind_stack.len() - remainder;
                s.extend_from_slice(&rewind_stack[lower..]);
            }
            s
        };

        // Build a new memory stack using the current stack and the data that was supplied
        let mut memory_stack = {
            let upper = snapshot.upper_memory_stack.len()
                .min(snapshot.memory_offset as usize);
            let mut s = snapshot.upper_memory_stack[..upper].to_vec();
            if snapshot.memory_offset as usize > upper {
                let remainder = snapshot.memory_offset as usize - upper;
                if remainder >= memory_stack.len() {
                    warn!("wasi::stack_restore - current memory stack is too small to restore the snapshot");
                    return OnCalledAction::Trap(Box::new(WasiError::Exit(__WASI_EFAULT as u32)));
                }
                let lower = memory_stack.len() - remainder;
                s.extend_from_slice(&memory_stack[lower..]);
            }
            s
        };
        
        // We compute the hash again for two reasons... integrity so if there
        // is a long jump that goes to the wrong place it will fail gracefully.
        // and security so that the stack can not be used to attempt to break
        // out of the sandbox
        let hash = {
            use sha2::{Sha256, Digest};
            let mut hasher = Sha256::new();
            hasher.update(&secret[..]);
            hasher.update(&memory_stack[..]);
            hasher.update(&rewind_stack[..]);
            let hash: [u8; 16] = hasher.finalize()[..16].try_into().unwrap();
            u128::from_le_bytes(hash)
        };
        if hash != snapshot.hash {
            warn!("snapshot hash failed - the snapshot can not be restored ({} vs {})", hash, snapshot.hash);
            return OnCalledAction::Trap(Box::new(WasiError::Exit(__WASI_EFAULT as u32)));
        }

        // If the return value offset is within the memory stack then we need
        // to update it here rather than in the real memory
        let ret_val_offset = snapshot.user;
        if ret_val_offset >= env.stack_start && ret_val_offset < env.stack_base
        {
            // Make sure its within the "active" part of the memory stack
            let offset = env.stack_base - ret_val_offset;
            if offset as usize > memory_stack.len() {
                warn!("snapshot failed - the return value is outside of the active part of the memory stack ({} vs {})", offset, memory_stack.len());
                return OnCalledAction::Trap(Box::new(WasiError::Exit(__WASI_EFAULT as u32)));
            }
            
            // Update the memory stack with the new return value
            let val_bytes = val.to_ne_bytes();
            let pstart = memory_stack.len() - offset as usize;
            let pend = pstart + val_bytes.len();
            let pbytes = &mut memory_stack[pstart..pend];
            pbytes.clone_from_slice(&val_bytes);
        } else {
            // Return return value must be on the stack otherwise it will cause
            // multithreading issues
            warn!("snapshot failed - the return value is not being returned on the stack - which is not supported");
            return OnCalledAction::Trap(Box::new(WasiError::Exit(__WASI_EFAULT as u32)));
        }

        // Rewind the stack - after this point we must immediately return
        // so that the execution can end here and continue elsewhere.
        match rewind::<M>(ctx, memory_stack, rewind_stack) {
            __WASI_ESUCCESS => OnCalledAction::InvokeAgain,
            err => {
                warn!("failed to rewind the stack - errno={}", err);
                OnCalledAction::Trap(Box::new(WasiError::Exit(__WASI_EFAULT as u32)))
            }
        }
    });

    // Return so the stack can be unwound (which will then
    // be rewound again but with a different location)
    Ok(())
}

/// ### `proc_signal()`
/// Sends a signal to a child process
///
/// ## Parameters
///
/// * `pid` - Handle of the child process to wait on
/// * `sig` - Signal to send the child process
pub fn proc_signal<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    pid: __wasi_pid_t,
    sig: __wasi_signal_t,
) -> Result<__wasi_errno_t, WasiError> {
    trace!("wasi::proc_signal");
    panic!("child signal not yet implemented")
}

/// Replaces the current process with a new process
///
/// ## Parameters
///
/// * `name` - Name of the process to be spawned
/// * `args` - List of the arguments to pass the process
///   (entries are separated by line feeds)
///
/// ## Return
///
/// Returns a bus process id that can be used to invoke calls
pub fn proc_exec<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    name: WasmPtr<u8, M>,
    name_len: M::Offset,
    args: WasmPtr<u8, M>,
    args_len: M::Offset,
) -> Result<(), WasiError> {
    trace!("wasi::proc_exec");
    panic!("fork not yet implemented")
}

/// ### `random_get()`
/// Fill buffer with high-quality random data.  This function may be slow and block
/// Inputs:
/// - `void *buf`
///     A pointer to a buffer where the random bytes will be written
/// - `size_t buf_len`
///     The number of bytes that will be written
pub fn random_get<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    buf: WasmPtr<u8, M>,
    buf_len: M::Offset,
) -> __wasi_errno_t {
    trace!("wasi::random_get buf_len: {}", buf_len);
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let buf_len64: u64 = buf_len.into();
    let mut u8_buffer = vec![0; buf_len64 as usize];
    let res = getrandom::getrandom(&mut u8_buffer);
    match res {
        Ok(()) => {
            let buf = wasi_try_mem!(buf.slice(&memory, buf_len));
            wasi_try_mem!(buf.write_slice(&u8_buffer));
            __WASI_ESUCCESS
        }
        Err(_) => __WASI_EIO,
    }
}

/// ### `tty_get()`
/// Retrieves the current state of the TTY
pub fn tty_get<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    tty_state: WasmPtr<__wasi_tty_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::tty_stdin");
    let env = ctx.data();

    let state = env.runtime.tty_get();
    let state = __wasi_tty_t {
        cols: state.cols,
        rows: state.rows,
        width: state.width,
        height: state.height,
        stdin_tty: match state.stdin_tty {
            false => __WASI_BOOL_FALSE,
            true => __WASI_BOOL_TRUE,
        },
        stdout_tty: match state.stdout_tty {
            false => __WASI_BOOL_FALSE,
            true => __WASI_BOOL_TRUE,
        },
        stderr_tty: match state.stderr_tty {
            false => __WASI_BOOL_FALSE,
            true => __WASI_BOOL_TRUE,
        },
        echo: match state.echo {
            false => __WASI_BOOL_FALSE,
            true => __WASI_BOOL_TRUE,
        },
        line_buffered: match state.line_buffered {
            false => __WASI_BOOL_FALSE,
            true => __WASI_BOOL_TRUE,
        },
    };

    let memory = env.memory_view(&ctx);
    wasi_try_mem!(tty_state.write(&memory, state));

    __WASI_ESUCCESS
}

/// ### `tty_set()`
/// Updates the properties of the rect
pub fn tty_set<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    tty_state: WasmPtr<__wasi_tty_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::tty_set");

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let state = wasi_try_mem!(tty_state.read(&memory));
    let state = super::runtime::WasiTtyState {
        cols: state.cols,
        rows: state.rows,
        width: state.width,
        height: state.height,
        stdin_tty: match state.stdin_tty {
            __WASI_BOOL_FALSE => false,
            __WASI_BOOL_TRUE => true,
            _ => return __WASI_EINVAL,
        },
        stdout_tty: match state.stdout_tty {
            __WASI_BOOL_FALSE => false,
            __WASI_BOOL_TRUE => true,
            _ => return __WASI_EINVAL,
        },
        stderr_tty: match state.stderr_tty {
            __WASI_BOOL_FALSE => false,
            __WASI_BOOL_TRUE => true,
            _ => return __WASI_EINVAL,
        },
        echo: match state.echo {
            __WASI_BOOL_FALSE => false,
            __WASI_BOOL_TRUE => true,
            _ => return __WASI_EINVAL,
        },
        line_buffered: match state.line_buffered {
            __WASI_BOOL_FALSE => false,
            __WASI_BOOL_TRUE => true,
            _ => return __WASI_EINVAL,
        },
    };

    env.runtime.tty_set(state);

    __WASI_ESUCCESS
}

/// ### `getcwd()`
/// Returns the current working directory
/// If the path exceeds the size of the buffer then this function
/// will fill the path_len with the needed size and return EOVERFLOW
pub fn getcwd<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    path: WasmPtr<u8, M>,
    path_len: WasmPtr<M::Offset, M>,
) -> __wasi_errno_t {
    debug!("wasi::getcwd");
    let env = ctx.data();
    let (memory, mut state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

    let (_, cur_dir) = wasi_try!(state
        .fs
        .get_current_dir(inodes.deref_mut(), crate::VIRTUAL_ROOT_FD,));

    let max_path_len = wasi_try_mem!(path_len.read(&memory));
    let path_slice = wasi_try_mem!(path.slice(&memory, max_path_len));
    let max_path_len: u64 = max_path_len.into();

    let cur_dir = cur_dir.as_bytes();
    wasi_try_mem!(path_len.write(&memory, wasi_try!(to_offset::<M>(cur_dir.len()))));
    if cur_dir.len() as u64 >= max_path_len {
        return __WASI_EOVERFLOW;
    }

    let cur_dir = {
        let mut u8_buffer = vec![0; max_path_len as usize];
        let cur_dir_len = cur_dir.len();
        if (cur_dir_len as u64) < max_path_len {
            u8_buffer[..cur_dir_len].clone_from_slice(cur_dir);
            u8_buffer[cur_dir_len] = 0;
        } else {
            return __WASI_EOVERFLOW;
        }
        u8_buffer
    };

    wasi_try_mem!(path_slice.write_slice(&cur_dir[..]));
    __WASI_ESUCCESS
}

/// ### `chdir()`
/// Sets the current working directory
pub fn chdir<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    path: WasmPtr<u8, M>,
    path_len: M::Offset,
) -> __wasi_errno_t {
    debug!("wasi::chdir");
    let env = ctx.data();
    let (memory, mut state) = env.get_memory_and_wasi_state(&ctx, 0);
    let path = unsafe { get_input_str!(&memory, path, path_len) };

    state.fs.set_current_dir(path.as_str());
    __WASI_ESUCCESS
}

/// ### `callback_spawn()`
/// Sets the callback to invoke upon spawning of new threads
/// 
/// ### Parameters
/// 
/// * `name` - Name of the function that will be invoked
pub fn callback_thread<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    name: WasmPtr<u8, M>,
    name_len: M::Offset,
) -> Result<(), MemoryAccessError> {
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let name = unsafe { name.read_utf8_string(&memory, name_len)? };
    debug!("wasi::callback_spawn (name={})", name);

    let funct = env.inner().exports
        .get_typed_function(&ctx, &name).ok();

    ctx.data_mut().inner_mut().thread_spawn = funct;
    Ok(())
}

/// ### `callback_signal()`
/// Sets the callback to invoke signals
/// 
/// ### Parameters
/// 
/// * `name` - Name of the function that will be invoked
pub fn callback_signal<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    name: WasmPtr<u8, M>,
    name_len: M::Offset,
) -> Result<(), WasiError> {
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let name = unsafe {
        match name.read_utf8_string(&memory, name_len) {
            Ok(a) => a,
            Err(err) => {
                warn!("failed to access memory that holds the name of the signal callback: {}", err);
                return Ok(());
            }
        }
    };
    debug!("wasi::callback_signal (name={})", name);

    let funct = env.inner().exports
        .get_typed_function(&ctx, &name).ok();

    ctx.data_mut().inner_mut().signal = funct;

    let env = ctx.data();
    env.clone().yield_now_with_signals(&mut ctx)?;

    Ok(())
}

/// ### `callback_reactor()`
/// Sets the callback to invoke for reactors
/// 
/// ### Parameters
/// 
/// * `name` - Name of the function that will be invoked
pub fn callback_reactor<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    name: WasmPtr<u8, M>,
    name_len: M::Offset,
) -> Result<(), MemoryAccessError> {
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let name = unsafe { name.read_utf8_string(&memory, name_len)? };
    debug!("wasi::callback_reactor (name={})", name);

    let funct = env.inner().exports
        .get_typed_function(&ctx, &name).ok();

    ctx.data_mut().inner_mut().react = funct;
    Ok(())
}

/// ### `callback_thread_local_destroy()`
/// Sets the callback to invoke for the destruction of thread local variables
/// 
/// ### Parameters
/// 
/// * `name` - Name of the function that will be invoked
pub fn callback_thread_local_destroy<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    name: WasmPtr<u8, M>,
    name_len: M::Offset,
) -> Result<(), MemoryAccessError> {
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let name = unsafe { name.read_utf8_string(&memory, name_len)? };
    debug!("wasi::callback_thread_local_destroy (name={})", name);

    let funct = env.inner().exports
        .get_typed_function(&ctx, &name).ok();

    ctx.data_mut().inner_mut().thread_local_destroy = funct;
    Ok(())
}

/// ### `thread_spawn()`
/// Creates a new thread by spawning that shares the same
/// memory address space, file handles and main event loops.
/// The function referenced by the fork call must be
/// exported by the web assembly process.
///
/// ## Parameters
///
/// * `name` - Name of the function that will be invoked as a new thread
/// * `user_data` - User data that will be supplied to the function when its called
/// * `reactor` - Indicates if the function will operate as a reactor or
///   as a normal thread. Reactors will be repeatable called
///   whenever IO work is available to be processed.
///
/// ## Return
///
/// Returns the thread index of the newly created thread
/// (indices always start from zero)
pub fn thread_spawn<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    user_data: u64,
    stack_base: u64,
    stack_start: u64,
    reactor: __wasi_bool_t,
    ret_tid: WasmPtr<__wasi_tid_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::thread_spawn (reactor={}, thread_id={}, stack_base={}, caller_id={})", reactor, ctx.data().id.raw(), stack_base, current_caller_id().raw());
    
    // Now we use the environment and memory references
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let runtime = env.runtime.clone();

    // Create the handle that represents this thread
    let mut thread_handle = {
        let mut guard = env.state.threading.write().unwrap();
        guard.new_thread()
    };
    let thread_id: __wasi_tid_t = thread_handle.id().into();

    // We need a copy of the process memory and a packaged store in order to
    // launch threads and reactors
    let thread_memory = wasi_try!(
        ctx.data()
            .memory()
            .try_clone(&ctx)
            .ok_or_else(|| {
                error!("thread failed - the memory could not be cloned");
                __WASI_ENOTCAPABLE
            })
    );
    #[cfg(feature = "compiler")]
    let engine = ctx.as_store_ref().engine().clone();

    // Build a new store that will be passed to the thread
    #[cfg(feature = "compiler")]
    let mut store = Store::new(engine);
    #[cfg(not(feature = "compiler"))]
    let mut store = Store::default();

    // This function takes in memory and a store and creates a context that
    // can be used to call back into the process
    let create_ctx = {
        let state = env.state.clone();
        let wasi_env = env.clone();
        let thread_id = thread_handle.id();
        move |mut store: Store, module: Module, memory: VMMemory|
        {
            // We need to reconstruct some things
            let module = module.clone();
            let memory = Memory::new_from_existing(&mut store, memory);

            // Build the context object and import the memory
            let mut ctx = WasiFunctionEnv::new(&mut store, wasi_env.clone());
            {
                let env = ctx.data_mut(&mut store);
                env.id = thread_id;
                env.stack_base = stack_base;
                env.stack_start = stack_start;
            }

            let mut import_object = import_object_for_all_wasi_versions(&mut store, &ctx.env);
            import_object.define("env", "memory", memory.clone());
            
            let instance = match Instance::new(&mut store, &module, &import_object) {
                Ok(a) => a,
                Err(err) => {
                    error!("thread failed - clone instance failed: {}", err);
                    return Err(__WASI_ENOEXEC as u32);
                }
            };
            
            // Set the current thread ID
            ctx.data_mut(&mut store).inner = Some(
                WasiEnvInner {
                    module,
                    memory,
                    exports: instance.exports.clone(),
                    stack_pointer: instance.exports.get_global("__stack_pointer").map(|a| a.clone()).ok(),
                    thread_spawn: instance.exports.get_typed_function(&store, "_start_thread").ok(),
                    react: instance.exports.get_typed_function(&store, "_react").ok(),
                    signal: instance.exports.get_typed_function(&store, "__wasm_signal").ok(),
                    asyncify_start_unwind: instance.exports.get_typed_function(&store, "asyncify_start_unwind").ok(),
                    asyncify_stop_unwind: instance.exports.get_typed_function(&store, "asyncify_stop_unwind").ok(),
                    asyncify_start_rewind: instance.exports.get_typed_function(&store, "asyncify_start_rewind").ok(),
                    asyncify_stop_rewind: instance.exports.get_typed_function(&store, "asyncify_stop_rewind").ok(),
                    asyncify_get_state: instance.exports.get_typed_function(&store, "asyncify_get_state").ok(),
                    thread_local_destroy: instance.exports.get_typed_function(&store, "_thread_local_destroy").ok(),
                }
            );
            trace!("threading: new context created for thread_id = {}", thread_id.raw());
            Ok(WasiThreadContext {
                ctx,
                store: RefCell::new(store)
            })
        }
    };

    // This function calls into the module
    let call_module = move |ctx: &WasiFunctionEnv, store: &mut Store|
    {
        // We either call the reactor callback or the thread spawn callback
        //trace!("threading: invoking thread callback (reactor={})", reactor);
        let spawn = match reactor {
            __WASI_BOOL_FALSE => ctx.data(&store).inner().thread_spawn.clone().unwrap(),
            __WASI_BOOL_TRUE => ctx.data(&store).inner().react.clone().unwrap(),
            _ => {
                debug!("thread failed - failed as the reactor type is not value");
                return __WASI_ENOEXEC as u32;
            }
        };

        let user_data_low: u32 = (user_data & 0xFFFFFFFF) as u32;
        let user_data_high: u32 = (user_data >> 32) as u32;

        let mut ret = __WASI_ESUCCESS;
        if let Err(err) = spawn.call(store, user_data_low as i32, user_data_high as i32) {
            debug!("thread failed - start: {}", err);
            ret = __WASI_ENOEXEC;
        }
        //trace!("threading: thread callback finished (reactor={}, ret={})", reactor, ret);
        
        // If we are NOT a reactor then we will only run once and need to clean up
        if reactor == __WASI_BOOL_FALSE
        {
            trace!("threading: cleaning up local thread variables");

            // Destroy all the local thread variables that were allocated for this thread
            let to_local_destroy = {
                let thread_id = ctx.data(store).id;
                let mut to_local_destroy = Vec::new();
                let mut guard = ctx.data(store).state.threading.write().unwrap();
                for ((thread, key), val) in guard.thread_local.iter() {
                    if *thread == thread_id {
                        if let Some(user_data) = guard.thread_local_user_data.get(key) {
                            to_local_destroy.push((*user_data, *val))
                        }
                    }
                }
                guard.thread_local.retain(|(t, _), _| *t != thread_id);
                to_local_destroy
            };
            if to_local_destroy.len() > 0 {
                if let Some(thread_local_destroy) = ctx.data(store).inner().thread_local_destroy.as_ref().map(|a| a.clone()) {
                    for (user_data, val) in to_local_destroy {
                        let user_data_low: u32 = (user_data & 0xFFFFFFFF) as u32;
                        let user_data_high: u32 = (user_data >> 32) as u32;

                        let val_low: u32 = (val & 0xFFFFFFFF) as u32;
                        let val_high: u32 = (val >> 32) as u32;

                        let _ = thread_local_destroy.call(store, user_data_low as i32, user_data_high as i32, val_low as i32, val_high as i32);
                    }
                }
            }
        }

        // Return the result
        ret as u32
    };

    // This next function gets a context for the local thread and then
    // calls into the process    
    let mut execute_module = {
        let state = env.state.clone();
        move |store: &mut Option<Store>, module: Module, memory: &mut Option<VMMemory>|
        {
            // We capture the thread handle here, it is used to notify
            // anyone that is interested when this thread has terminated
            let _captured_handle = Box::new(&mut thread_handle);

            // Given that it is not safe to assume this delegate will run on the
            // same thread we need to capture a simple process that will create
            // context objects on demand and reuse them
            let caller_id = current_caller_id();

            // We loop because read locks are held while functions run which need
            // to be relocked in the case of a miss hit.
            loop {
                let thread = {
                    let guard = state.threading.read().unwrap();
                    guard.thread_ctx.get(&caller_id).map(|a| a.clone())
                };
                if let Some(thread) = thread
                {
                    let mut store = thread.store.borrow_mut();
                    let ret = call_module(&thread.ctx, store.deref_mut());
                    return ret;
                }

                // Otherwise we need to create a new context under a write lock
                debug!("encountered a new caller (ref={}) - creating WASM execution context...", caller_id.raw());

                // We can only create the context once per thread
                let memory = match memory.take() {
                    Some(m) => m,
                    None => {
                        debug!("thread failed - memory can only be consumed once per context creation");
                        return __WASI_ENOEXEC as u32;
                    }
                };
                let store = match store.take() {
                    Some(s) => s,
                    None => {
                        debug!("thread failed - store can only be consumed once per context creation");
                        return __WASI_ENOEXEC as u32;
                    }
                };

                // Now create the context and hook it up
                let mut guard = state.threading.write().unwrap();
                let ctx = match create_ctx(store, module.clone(), memory) {
                    Ok(c) => c,
                    Err(err) => {
                        return err;
                    }
                };
                guard.thread_ctx.insert(caller_id, Arc::new(ctx));
            }
        }
    };

    // If we are a reactor then instead of launching the thread now
    // we store it in the state machine and only launch it whenever
    // work arrives that needs to be processed
    match reactor {
        __WASI_BOOL_TRUE => {
            warn!("thread failed - reactors are not currently supported");
            return __WASI_ENOTCAPABLE;
        },
        __WASI_BOOL_FALSE => {
            // If the process does not export a thread spawn function then obviously
            // we can't spawn a background thread
            if env.inner().thread_spawn.is_none() {
                warn!("thread failed - the program does not export a _start_thread function");
                return __WASI_ENOTCAPABLE;
            }

            // Now spawn a thread
            trace!("threading: spawning background thread");
            let thread_module = env.inner().module.clone();
            wasi_try!(runtime
                .thread_spawn(Box::new(move |store: Store, module: Module, thread_memory: VMMemory| {
                    let mut thread_memory = Some(thread_memory);
                    let mut store = Some(store);
                    execute_module(&mut store, module, &mut thread_memory);
                }), store, thread_module, thread_memory)
                .map_err(|err| {
                    let err: __wasi_errno_t = err.into();
                    err
                }));
        },
        _ => {
            warn!("thread failed - invalid reactor parameter value");
            return __WASI_ENOTCAPABLE;
        }
    }
    
    // Success
    let memory = ctx.data().memory_view(&ctx);
    wasi_try_mem!(ret_tid.write(&memory, thread_id));
    __WASI_ESUCCESS
}

/// ### `thread_local_create()`
/// Create a thread local variable
/// If The web assembly process exports function named '_thread_local_destroy'
/// then it will be invoked when the thread goes out of scope and dies.
///
/// ## Parameters
///
/// * `user_data` - User data that will be passed to the destructor
///   when the thread variable goes out of scope
pub fn thread_local_create<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    user_data: u64,
    ret_key: WasmPtr<__wasi_tl_key_t, M>,
) -> __wasi_errno_t {
    trace!("wasi::thread_local_create (user_data={})", user_data);
    let env = ctx.data();

    let key = {
        let mut guard = env.state.threading.write().unwrap();
        guard.thread_local_seed += 1;
        let key = guard.thread_local_seed;
        guard.thread_local_user_data.insert(key, user_data);
        key
    };
    
    let memory = env.memory_view(&ctx);
    wasi_try_mem!(ret_key.write(&memory, key));
    __WASI_ESUCCESS
}

/// ### `thread_local_destroy()`
/// Destroys a thread local variable
///
/// ## Parameters
///
/// * `user_data` - User data that will be passed to the destructor
///   when the thread variable goes out of scope
/// * `key` - Thread key that was previously created
pub fn thread_local_destroy(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    key: __wasi_tl_key_t
) -> __wasi_errno_t {
    trace!("wasi::thread_local_destroy (key={})", key);
    let state = ctx.data().state.clone();
    let mut guard = state.threading.write().unwrap();
    if let Some(user_data) = guard.thread_local_user_data.remove(&key) {
        if let Some(thread_local_destroy) = ctx.data().inner().thread_local_destroy.as_ref().map(|a| a.clone()) {
            guard.thread_local
                .iter()
                .filter(|((_, k), _)| *k == key)
                .for_each(|((_, _), val)| {
                    let user_data_low: u32 = (user_data & 0xFFFFFFFF) as u32;
                    let user_data_high: u32 = (user_data >> 32) as u32;

                    let val_low: u32 = (val & 0xFFFFFFFF) as u32;
                    let val_high: u32 = (val >> 32) as u32;

                    let _ = thread_local_destroy.call(&mut ctx, user_data_low as i32, user_data_high as i32, val_low as i32, val_high as i32);
                });
        }
    }
    guard.thread_local.retain(|(_, k), _| *k != key);
    __WASI_ESUCCESS
}

/// ### `thread_local_set()`
/// Sets the value of a thread local variable
///
/// ## Parameters
///
/// * `key` - Thread key that this local variable will be associated with
/// * `val` - Value to be set for the thread local variable
pub fn thread_local_set(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    key: __wasi_tl_key_t,
    val: __wasi_tl_val_t
) -> __wasi_errno_t {
    //trace!("wasi::thread_local_set (key={}, val={})", key, val);
    let env = ctx.data();

    let current_thread = ctx.data().id;
    let mut guard = env.state.threading.write().unwrap();
    guard.thread_local.insert((current_thread, key), val);
    __WASI_ESUCCESS
}

/// ### `thread_local_get()`
/// Gets the value of a thread local variable
///
/// ## Parameters
///
/// * `key` - Thread key that this local variable that was previous set
pub fn thread_local_get<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    key: __wasi_tl_key_t,
    ret_val: WasmPtr<__wasi_tl_val_t, M>,
) -> __wasi_errno_t {
    //trace!("wasi::thread_local_get (key={})", key);
    let env = ctx.data();

    let val = {
        let current_thread = ctx.data().id;
        let guard = env.state.threading.read().unwrap();
        guard.thread_local.get(&(current_thread, key)).map(|a| a.clone())
    };
    let val = val.unwrap_or_default();
    let memory = env.memory_view(&ctx);
    wasi_try_mem!(ret_val.write(&memory, val));
    __WASI_ESUCCESS
}

/// ### `thread_sleep()`
/// Sends the current thread to sleep for a period of time
///
/// ## Parameters
///
/// * `duration` - Amount of time that the thread should sleep
pub fn thread_sleep(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    duration: __wasi_timestamp_t,
) -> Result<__wasi_errno_t, WasiError> {
    //trace!("wasi::thread_sleep");

    let env = ctx.data();
    let duration = Duration::from_nanos(duration as u64);
    env.clone().sleep(&mut ctx, duration)?;
    Ok(__WASI_ESUCCESS)
}

/// ### `thread_id()`
/// Returns the index of the current thread
/// (threads indices are sequencial from zero)
pub fn thread_id<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    ret_tid: WasmPtr<__wasi_tid_t, M>,
) -> __wasi_errno_t {
    //trace!("wasi::thread_id");

    let env = ctx.data();
    let tid: __wasi_tid_t = env.id.into();
    let memory = env.memory_view(&ctx);
    wasi_try_mem!(ret_tid.write(&memory, tid));
    __WASI_ESUCCESS
}

/// ### `thread_join()`
/// Joins this thread with another thread, blocking this
/// one until the other finishes
///
/// ## Parameters
///
/// * `tid` - Handle of the thread to wait on
pub fn thread_join(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    tid: __wasi_tid_t,
) -> Result<__wasi_errno_t, WasiError> {
    debug!("wasi::thread_join");

    let env = ctx.data();
    let tid: WasiThreadId = tid.into();
    let other_thread = {
        let guard = env.state.threading.read().unwrap();
        guard.get(&tid)
    };
    if let Some(other_thread) = other_thread {
        loop {
            env.yield_now()?;
            if other_thread.join(Duration::from_millis(50)) {
                break;
            }
        }
        Ok(__WASI_ESUCCESS)
    } else {
        Ok(__WASI_ESUCCESS)
    }
}

/// ### `thread_parallelism()`
/// Returns the available parallelism which is normally the
/// number of available cores that can run concurrently
pub fn thread_parallelism<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    ret_parallelism: WasmPtr<M::Offset, M>,
) -> __wasi_errno_t {
    debug!("wasi::thread_parallelism");

    let env = ctx.data();
    let parallelism = wasi_try!(env.runtime().thread_parallelism().map_err(|err| {
        let err: __wasi_errno_t = err.into();
        err
    }));
    let parallelism: M::Offset = wasi_try!(parallelism.try_into().map_err(|_| __WASI_EOVERFLOW));
    let memory = env.memory_view(&ctx);
    wasi_try_mem!(ret_parallelism.write(&memory, parallelism));
    __WASI_ESUCCESS
}

/// Wait for a futex_wake operation to wake us.
/// Returns with EINVAL if the futex doesn't hold the expected value.
/// Returns false on timeout, and true in all other cases.
///
/// ## Parameters
///
/// * `futex` - Memory location that holds the value that will be checked
/// * `expected` - Expected value that should be currently held at the memory location
/// * `timeout` - Timeout should the futex not be triggered in the allocated time
pub fn futex_wait<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    futex_ptr: WasmPtr<u32, M>,
    expected: u32,
    timeout: WasmPtr<__wasi_option_timestamp_t, M>,
    ret_woken: WasmPtr<__wasi_bool_t, M>,
) -> Result<__wasi_errno_t, WasiError> {
    trace!("wasi::futex_wait(offset={})", futex_ptr.offset());
    let env = ctx.data();
    let state = env.state.deref();

    let pointer: u64 = wasi_try_ok!(futex_ptr.offset().try_into().map_err(|_| __WASI_EOVERFLOW));

    // Register the waiting futex
    let futex = {
        use std::collections::hash_map::Entry;
        let mut guard = state.futexs.lock().unwrap();
        match guard.entry(pointer) {
            Entry::Occupied(entry) => {
                entry.get().clone()
            },
            Entry::Vacant(entry) => {
                let futex = WasiFutex {
                    refcnt: Arc::new(AtomicU32::new(1)),
                    inner: Arc::new((Mutex::new(()), Condvar::new()))
                };
                entry.insert(futex.clone());
                futex
            }
        }
    };

    // Loop until we either hit a yield error or the futex is woken
    let mut yielded = Ok(());
    loop {
        let futex_lock = futex.inner.0.lock().unwrap();

        // If the value of the memory is no longer the expected value
        // then terminate from the loop (we do this under a futex lock
        // so that its protected)
        {
            let view = env.memory_view(&ctx);
            let val = wasi_try_mem_ok!(futex_ptr.read(&view));
            if val != expected {
                break;
            }
        }

        let result = futex.inner.1.wait_timeout(futex_lock, Duration::from_millis(50)).unwrap();
        if result.1.timed_out() {
            yielded = env.yield_now();
            if yielded.is_err() {
                break;
            }
        } else {
            break;
        }
    }

    // Drop the reference count to the futex (and remove it if the refcnt hits zero)
    {
        let mut guard = state.futexs.lock().unwrap();
        if guard.get(&pointer)
            .map(|futex| futex.refcnt.fetch_sub(1, Ordering::AcqRel) == 1)
            .unwrap_or(false)
        {
            guard.remove(&pointer);
        }
    }

    // We may have a yield error (such as a terminate)
    yielded?;

    Ok(__WASI_ESUCCESS)
}

/// Wake up one thread that's blocked on futex_wait on this futex.
/// Returns true if this actually woke up such a thread,
/// or false if no thread was waiting on this futex.
///
/// ## Parameters
///
/// * `futex` - Memory location that holds a futex that others may be waiting on
pub fn futex_wake<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    futex: WasmPtr<u32, M>,
    ret_woken: WasmPtr<__wasi_bool_t, M>,
) -> __wasi_errno_t {
    trace!("wasi::futex_wake(offset={})", futex.offset());
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let state = env.state.deref();
    
    let pointer: u64 = wasi_try!(futex.offset().try_into().map_err(|_| __WASI_EOVERFLOW));
    let mut woken = false;

    let mut guard = state.futexs.lock().unwrap();
    if let Some(futex) = guard.get(&pointer) {
        futex.inner.1.notify_one();
        woken = true;
    } else {
        trace!("wasi::futex_wake - nothing waiting!");
    }

    let woken = match woken {
        false => __WASI_BOOL_FALSE,
        true => __WASI_BOOL_TRUE,
    };
    wasi_try_mem!(ret_woken.write(&memory, woken));
    
    __WASI_ESUCCESS
}

/// Wake up all threads that are waiting on futex_wait on this futex.
///
/// ## Parameters
///
/// * `futex` - Memory location that holds a futex that others may be waiting on
pub fn futex_wake_all<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    futex: WasmPtr<u32, M>,
    ret_woken: WasmPtr<__wasi_bool_t, M>,
) -> __wasi_errno_t {
    trace!("wasi::futex_wake_all(offset={})", futex.offset());
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let state = env.state.deref();

    let pointer: u64 = wasi_try!(futex.offset().try_into().map_err(|_| __WASI_EOVERFLOW));
    let mut woken = false;

    let mut guard = state.futexs.lock().unwrap();
    if let Some(futex) = guard.remove(&pointer) {
        futex.inner.1.notify_all();
        woken = true;
    }

    let woken = match woken {
        false => __WASI_BOOL_FALSE,
        true => __WASI_BOOL_TRUE,
    };
    wasi_try_mem!(ret_woken.write(&memory, woken));
    
    __WASI_ESUCCESS
}

/// ### `getpid()`
/// Returns the handle of the current process
pub fn proc_id<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    ret_pid: WasmPtr<__wasi_pid_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::getpid");

    let env = ctx.data();
    let pid = env.runtime().getpid();
    if let Some(pid) = pid {
        let memory = env.memory_view(&ctx);
        wasi_try_mem!(ret_pid.write(&memory, pid as __wasi_pid_t));
        __WASI_ESUCCESS
    } else {
        __WASI_ENOTSUP
    }
}

/// ### `getppid()`
/// Returns the parent handle of the supplied process
pub fn proc_parent<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    pid: __wasi_pid_t,
    ret_parent: WasmPtr<__wasi_pid_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::getpid");

    let env = ctx.data();
    
    let parent = env.runtime().getppid(pid);

    if let Some(parent) = parent {
        let memory = env.memory_view(&ctx);
        wasi_try_mem!(ret_parent.write(&memory, parent as __wasi_pid_t));
        __WASI_ESUCCESS
    } else {
        __WASI_ENOTSUP
    }
}

/// ### `thread_exit()`
/// Terminates the current running thread, if this is the last thread then
/// the process will also exit with the specified exit code. An exit code
/// of 0 indicates successful termination of the thread. The meanings of
/// other values is dependent on the environment.
///
/// ## Parameters
///
/// * `rval` - The exit code returned by the process.
pub fn thread_exit(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    exitcode: __wasi_exitcode_t,
) -> Result<(), WasiError> {
    debug!("wasi::thread_exit");
    Err(WasiError::Exit(exitcode))
}

/// ### `proc_fork()`
/// Forks the current process into a new subprocess. If the function
/// returns a zero then its the new subprocess. If it returns a positive
/// number then its the current process and the $pid represents the child.
pub fn proc_fork<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    pid_ptr: WasmPtr<__wasi_pid_t, M>,
) -> Result<__wasi_errno_t, WasiError> {
    // If we were just restored then we need to return the value instead
    if handle_rewind::<M>(&mut ctx) {
        let env = ctx.data();
        let memory = env.memory_view(&ctx);
        let pid = wasi_try_mem_ok!(
            pid_ptr.read(&memory)
        );
        trace!("wasi::proc_fork - restored - (pid={})", pid);
        return Ok(__WASI_ESUCCESS);
    }
    trace!("wasi::proc_fork - capturing");

    // We write a zero to the PID before we capture the stack
    // so that this is what will be returned to the child
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    wasi_try_mem_ok!(
        pid_ptr.write(&memory, 0)
    );

    // Pass some offsets to the unwind function
    let pid_offset = pid_ptr.offset();

    // Create the thread that will back this forked process
    let state = env.state.clone();
    let (pid, thread) = {
        let mut guard = state.threading.write().unwrap();
        let thread = guard.new_thread();
        guard.bus_process_seed += 1;
        let pid = guard.bus_process_seed;
        guard.bus_process_fork.insert(pid.into(), thread.id());
        (pid, thread)
    };

    // Perform the unwind action
    unwind::<M, _>(ctx, move |mut ctx, mut memory_stack, rewind_stack|
    {
        // Fork the memory and copy the module (compiled code)
        let env = ctx.data();
        let fork_memory = match env
                .memory()
                .try_clone(&ctx)
                .ok_or_else(|| {
                    error!("thread failed - the memory could not be cloned");
                    MemoryError::Generic(format!("the memory could not be cloned"))
                })
                .and_then(|mut memory| memory.fork())
        {
            Ok(memory) => {
                 memory.into()
            },
            Err(err) => {
                warn!("fork failed - could not fork the memory - {}", err);
                return OnCalledAction::Trap(Box::new(WasiError::Exit(__WASI_EFAULT as u32)));
            }
        };
        let fork_module = env.inner().module.clone();

        #[cfg(feature = "compiler")]
        let engine = ctx.as_store_ref().engine().clone();

        // Build a new store that will be passed to the thread
        #[cfg(feature = "compiler")]
        let mut fork_store = Store::new(engine);
        #[cfg(not(feature = "compiler"))]
        let mut fork_store = Store::default();

        // Spawn the process
        let runtime = env.runtime.clone();
        if let Err(error) = runtime
            .thread_spawn(Box::new(move |store: Store, module: Module, thread_memory: VMMemory| {
                error!("not yet implemented");
            }),
            fork_store,
            fork_module,
            fork_memory)
        {
            warn!("fork failed - could not spawn thread that will represent this forked process");
            return OnCalledAction::Trap(Box::new(WasiError::Exit(__WASI_EFAULT as u32)));
        }

        // If the return value offset is within the memory stack then we need
        // to update it here rather than in the real memory
        let pid_offset: u64 = pid_offset.into();
        if pid_offset >= env.stack_start && pid_offset < env.stack_base
        {
            // Make sure its within the "active" part of the memory stack
            let offset = env.stack_base - pid_offset;
            if offset as usize > memory_stack.len() {
                warn!("fork failed - the return value (pid) is outside of the active part of the memory stack ({} vs {})", offset, memory_stack.len());
                return OnCalledAction::Trap(Box::new(WasiError::Exit(__WASI_EFAULT as u32)));
            }
            
            // Update the memory stack with the new PID
            let val_bytes = pid.to_ne_bytes();
            let pstart = memory_stack.len() - offset as usize;
            let pend = pstart + val_bytes.len();
            let pbytes = &mut memory_stack[pstart..pend];
            pbytes.clone_from_slice(&val_bytes);
        } else {
            warn!("fork failed - the return value (pid) is not being returned on the stack - which is not supported");
            return OnCalledAction::Trap(Box::new(WasiError::Exit(__WASI_EFAULT as u32)));
        }

        // Rewind the stack and carry on
        match rewind::<M>(ctx, memory_stack, rewind_stack) {
            __WASI_ESUCCESS => OnCalledAction::InvokeAgain,
            err => {
                warn!("fork failed - could not rewind the stack - errno={}", err);
                OnCalledAction::Trap(Box::new(WasiError::Exit(__WASI_EFAULT as u32)))
            }
        }
    })
}

/// Spawns a new process within the context of this machine
///
/// ## Parameters
///
/// * `name` - Name of the process to be spawned
/// * `chroot` - Indicates if the process will chroot or not
/// * `args` - List of the arguments to pass the process
///   (entries are separated by line feeds)
/// * `preopen` - List of the preopens for this process
///   (entries are separated by line feeds)
/// * `stdin` - How will stdin be handled
/// * `stdout` - How will stdout be handled
/// * `stderr` - How will stderr be handled
/// * `working_dir` - Working directory where this process should run
///   (passing '.' will use the current directory)
///
/// ## Return
///
/// Returns a bus process id that can be used to invoke calls
pub fn proc_spawn<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    name: WasmPtr<u8, M>,
    name_len: M::Offset,
    chroot: __wasi_bool_t,
    args: WasmPtr<u8, M>,
    args_len: M::Offset,
    preopen: WasmPtr<u8, M>,
    preopen_len: M::Offset,
    stdin: __wasi_stdiomode_t,
    stdout: __wasi_stdiomode_t,
    stderr: __wasi_stdiomode_t,
    working_dir: WasmPtr<u8, M>,
    working_dir_len: M::Offset,
    ret_handles: WasmPtr<__wasi_bus_handles_t, M>,
) -> __bus_errno_t {
    let env = ctx.data();
    let bus = env.runtime.bus();
    let memory = env.memory_view(&ctx);
    let name = unsafe { get_input_str_bus!(&memory, name, name_len) };
    let args = unsafe { get_input_str_bus!(&memory, args, args_len) };
    let preopen = unsafe { get_input_str_bus!(&memory, preopen, preopen_len) };
    let working_dir = unsafe { get_input_str_bus!(&memory, working_dir, working_dir_len) };
    let chroot = chroot == __WASI_BOOL_TRUE;
    debug!("wasi::process_spawn (name={})", name);

    let args: Vec<_> = args.split(&['\n', '\r']).map(|a| a.to_string()).collect();

    let preopen: Vec<_> = preopen
        .split(&['\n', '\r'])
        .map(|a| a.to_string())
        .collect();

    let conv_stdio_mode = |mode: __wasi_stdiomode_t| match mode {
        __WASI_STDIO_MODE_PIPED => StdioMode::Piped,
        __WASI_STDIO_MODE_INHERIT => StdioMode::Inherit,
        __WASI_STDIO_MODE_LOG => StdioMode::Log,
        __WASI_STDIO_MODE_NULL | _ => StdioMode::Null,
    };

    let mut process = wasi_try_bus!(bus
        .new_spawn()
        .chroot(chroot)
        .args(args)
        .preopen(preopen)
        .stdin_mode(conv_stdio_mode(stdin))
        .stdout_mode(conv_stdio_mode(stdout))
        .stderr_mode(conv_stdio_mode(stderr))
        .working_dir(working_dir)
        .spawn(name.as_str())
        .map_err(bus_error_into_wasi_err));

    // Create the file descriptors used to access this process
    let (fd_stdin, fd_stdout, fd_stderr) = {
        let (memory, state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

        // Register the inodes for the stdio for this sub process
        let fd_stdin = match process.stdin.take() {
            Some(handle) => Some(
                wasi_try_bus!(state.fs.create_fd(bus_write_rights(), bus_write_rights(), 0, 0,
                    state.fs.create_inode_with_default_stat(
                        inodes.deref_mut(),
                        Kind::File { handle: Some(handle), path: "/dev/stdin".into(), fd: None },
                        false,
                        "stdin".into(),
                    )
                ).map_err(|_| __BUS_EINTERNAL))
            ),
            None => None,
        };
        let fd_stdout = match process.stdout.take() {
            Some(handle) => Some(
                wasi_try_bus!(state.fs.create_fd(bus_read_rights(), bus_read_rights(), 0, 0,
                    state.fs.create_inode_with_default_stat(
                        inodes.deref_mut(),
                        Kind::File { handle: Some(handle), path: "/dev/stdout".into(), fd: None },
                        false,
                        "stdout".into(),
                    )
                ).map_err(|_| __BUS_EINTERNAL))
            ),
            None => None
        };
        let fd_stderr = match process.stderr.take() {
            Some(handle) => Some(
                wasi_try_bus!(state.fs.create_fd(bus_read_rights(), bus_read_rights(), 0, 0,
                    state.fs.create_inode_with_default_stat(
                        inodes.deref_mut(),
                        Kind::File { handle: Some(handle), path: "/dev/stderr".into(), fd: None },
                        false,
                        "stderr".into(),
                    )
                ).map_err(|_| __BUS_EINTERNAL))
            ),
            None => None
        };
        (fd_stdin, fd_stdout, fd_stderr)
    };    

    let conv_stdio_fd = |a: Option<__wasi_fd_t>| match a {
        Some(fd) => __wasi_option_fd_t {
            tag: __WASI_OPTION_SOME,
            fd: fd.into(),
        },
        None => __wasi_option_fd_t {
            tag: __WASI_OPTION_NONE,
            fd: 0,
        },
    };
    
    // Convert the stdio
    let stdin = conv_stdio_fd(fd_stdin);
    let stdout = conv_stdio_fd(fd_stdout);
    let stderr = conv_stdio_fd(fd_stderr);

    // Add the process to the environment state
    let pid = {
        let mut guard = env.state.threading.write().unwrap();
        guard.bus_process_seed += 1;
        let bid = guard.bus_process_seed;
        guard.bus_processes.insert(bid.into(), Box::new(process));
        bid
    };

    let handles = __wasi_bus_handles_t {
        bid: pid,
        stdin,
        stdout,
        stderr,
    };

    wasi_try_mem_bus!(ret_handles.write(&memory, handles));

    __BUS_ESUCCESS
}

/// ### `proc_join()`
/// Joins the child process,blocking this one until the other finishes
///
/// ## Parameters
///
/// * `pid` - Handle of the child process to wait on
pub fn proc_join<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    pid: __wasi_pid_t,
    exit_code: WasmPtr<__wasi_exitcode_t, M>,
) -> Result<__wasi_errno_t, WasiError> {
    trace!("wasi::proc_join");
    panic!("join not yet implemented")
}

/// Spawns a new bus process for a particular web WebAssembly
/// binary that is referenced by its process name.
///
/// ## Parameters
///
/// * `name` - Name of the process to be spawned
/// * `reuse` - Indicates if the existing processes should be reused
///   if they are already running
///
/// ## Return
///
/// Returns a bus process id that can be used to invoke calls
pub fn bus_open_local<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    name: WasmPtr<u8, M>,
    name_len: M::Offset,
    reuse: __wasi_bool_t,
    ret_bid: WasmPtr<__wasi_bid_t, M>,
) -> Result<__bus_errno_t, WasiError> {
    let env = ctx.data();
    let bus = env.runtime.bus();
    let memory = env.memory_view(&ctx);
    let name = unsafe { get_input_str_bus_ok!(&memory, name, name_len) };
    let reuse = reuse == __WASI_BOOL_TRUE;
    debug!("wasi::bus_open_local (name={}, reuse={})", name, reuse);

    bus_open_internal(ctx, name, reuse, None, None, ret_bid)
}

/// Spawns a new bus process for a particular web WebAssembly
/// binary that is referenced by its process name on a remote instance.
///
/// ## Parameters
///
/// * `name` - Name of the process to be spawned
/// * `reuse` - Indicates if the existing processes should be reused
///   if they are already running
/// * `instance` - Instance identifier where this process will be spawned
/// * `token` - Acceess token used to authenticate with the instance
///
/// ## Return
///
/// Returns a bus process id that can be used to invoke calls
pub fn bus_open_remote<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    name: WasmPtr<u8, M>,
    name_len: M::Offset,
    reuse: __wasi_bool_t,
    instance: WasmPtr<u8, M>,
    instance_len: M::Offset,
    token: WasmPtr<u8, M>,
    token_len: M::Offset,
    ret_bid: WasmPtr<__wasi_bid_t, M>,
) -> Result<__bus_errno_t, WasiError> {
    let env = ctx.data();
    let bus = env.runtime.bus();
    let memory = env.memory_view(&ctx);
    let name = unsafe { get_input_str_bus_ok!(&memory, name, name_len) };
    let instance = unsafe { get_input_str_bus_ok!(&memory, instance, instance_len) };
    let token = unsafe { get_input_str_bus_ok!(&memory, token, token_len) };
    let reuse = reuse == __WASI_BOOL_TRUE;
    debug!(
        "wasi::bus_open_remote (name={}, reuse={}, instance={})",
        name, reuse, instance
    );

    bus_open_internal(ctx, name, reuse, Some(instance), Some(token), ret_bid)
}

fn bus_open_internal<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    name: String,
    reuse: bool,
    instance: Option<String>,
    token: Option<String>,
    ret_bid: WasmPtr<__wasi_bid_t, M>,
) -> Result<__bus_errno_t, WasiError> {
    let env = ctx.data();
    let bus = env.runtime.bus();
    let memory = env.memory_view(&ctx);
    let name: Cow<'static, str> = name.into();

    // Check if it already exists
    if reuse {
        let guard = env.state.threading.read().unwrap();
        if let Some(bid) = guard.bus_process_reuse.get(&name) {
            if guard.bus_processes.contains_key(bid) {
                wasi_try_mem_bus_ok!(ret_bid.write(&memory, bid.clone().into()));
                return Ok(__BUS_ESUCCESS);
            }
        }
    }

    let mut process = bus.new_spawn();
    process
        .reuse(reuse)
        .stdin_mode(StdioMode::Null)
        .stdout_mode(StdioMode::Null)
        .stderr_mode(StdioMode::Log);

    if let Some(instance) = instance {
        process.remote_instance(instance);
    }

    if let Some(token) = token {
        process.access_token(token);
    }

    // Spawn the process
    let mut process = wasi_try_bus_ok!(process
        .spawn(name.as_ref())
        .map_err(bus_error_into_wasi_err));

    // Add the process to the environment state
    let bid = {
        let mut guard = env.state.threading.write().unwrap();
        guard.bus_process_seed += 1;
        let bid: WasiBusProcessId = guard.bus_process_seed.into();
        guard.bus_processes.insert(bid, Box::new(process));
        guard.bus_process_reuse.insert(name, bid);
        bid
    };

    wasi_try_mem_bus_ok!(ret_bid.write(&memory, bid.into()));

    Ok(__BUS_ESUCCESS)
}

/// Closes a bus process and releases all associated resources
///
/// ## Parameters
///
/// * `bid` - Handle of the bus process handle to be closed
pub fn bus_close(ctx: FunctionEnvMut<'_, WasiEnv>, bid: __wasi_bid_t) -> __bus_errno_t {
    trace!("wasi::bus_close (bid={})", bid);
    let bid: WasiBusProcessId = bid.into();

    let env = ctx.data();
    let mut guard = env.state.threading.write().unwrap();
    if let Some(process) = guard.bus_processes.remove(&bid) {
        let name: Cow<'static, str> = process.name.clone().into();
        guard.bus_process_reuse.remove(&name);
    }

    __BUS_ESUCCESS
}

/// Invokes a call within a running bus process.
///
/// ## Parameters
///
/// * `bid` - Handle of the bus process to invoke the call within
/// * `keep_alive` - Causes the call handle to remain open even when A
///   reply is received. It is then the  callers responsibility
///   to invoke 'bus_drop' when they are finished with the call
/// * `topic` - Topic that describes the type of call to made
/// * `format` - Format of the data pushed onto the bus
/// * `buf` - The buffer where data to be transmitted is stored
pub fn bus_call<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    bid: __wasi_bid_t,
    topic_hash: WasmPtr<__wasi_hash_t>,
    format: __wasi_busdataformat_t,
    buf: WasmPtr<u8, M>,
    buf_len: M::Offset,
    ret_cid: WasmPtr<__wasi_cid_t, M>,
) -> Result<__bus_errno_t, WasiError> {
    let env = ctx.data();
    let bus = env.runtime.bus();
    let memory = env.memory_view(&ctx);
    let topic_hash = wasi_try_mem_bus_ok!(topic_hash.read(&memory));
    let buf_slice = wasi_try_mem_bus_ok!(buf.slice(&memory, buf_len));
    trace!(
        "wasi::bus_call (bid={}, buf_len={})",
        bid,
        buf_len
    );

    // Get the process that we'll invoke this call for
    let mut guard = env.state.threading.read().unwrap();
    let bid: WasiBusProcessId = bid.into();
    let process = if let Some(process) = {
        guard.bus_processes.get(&bid).map(|p| p.clone())
    } { process } else {
        return Ok(__BUS_EBADHANDLE);
    };

    // Invoke the bus process
    let format = wasi_try_bus_ok!(conv_bus_format_from(format));
    
    // Check if the process has finished
    if let Some(code) = process.inst.exit_code() {
        debug!("process has already exited (code = {})", code);
        return Ok(__BUS_EABORTED);
    }    

    // Invoke the call
    let buf = wasi_try_mem_bus_ok!(buf_slice.read_to_vec());
    let mut invoked = process.inst.invoke(topic_hash, format, buf);
    drop(process);
    drop(guard);

    // Poll the invocation until it does its thing
    let mut invocation;
    {
        // Fast path (does not have to create a futex creation)
        let waker = WasiDummyWaker.into_waker();
        let mut cx = Context::from_waker(&waker);
        let pinned_invoked = Pin::new(invoked.deref_mut());
        match pinned_invoked.poll_invoked(&mut cx) {
            Poll::Ready(i) => {
                invocation = wasi_try_bus_ok!(i
                    .map_err(bus_error_into_wasi_err));
            },
            Poll::Pending => {
                // Slow path (will put the thread to sleep)
                let parking = WasiParkingLot::default();
                let waker = parking.get_waker();
                let mut cx = Context::from_waker(&waker);
                loop {
                    let pinned_invoked = Pin::new(invoked.deref_mut());
                    match pinned_invoked.poll_invoked(&mut cx) {
                        Poll::Ready(i) => {
                            invocation = wasi_try_bus_ok!(i
                                .map_err(bus_error_into_wasi_err));
                            break;
                        },
                        Poll::Pending => {
                            env.yield_now()?;
                            parking.wait(Duration::from_millis(5));
                        }
                    }
                }       
            }
        }
    }

    // Record the invocation
    let cid = {
        let mut guard = env.state.bus.protected();
        guard.call_seed += 1;
        let cid = guard.call_seed;
        guard.calls.insert(cid, WasiBusCall {
            bid,
            invocation
        });
        cid
    };

    // Now we wake any BUS pollers so that they can drive forward the
    // call to completion - when they poll the call they will also
    // register a BUS waker
    env.state.bus.poll_wake();

    // Return the CID and success to the caller
    wasi_try_mem_bus_ok!(ret_cid.write(&memory, cid));
    Ok(__BUS_ESUCCESS)
}

/// Invokes a call within the context of another call
///
/// ## Parameters
///
/// * `parent` - Parent bus call that this is related to
/// * `keep_alive` - Causes the call handle to remain open even when A
///   reply is received. It is then the  callers responsibility
///   to invoke 'bus_drop' when they are finished with the call
/// * `topic` - Topic that describes the type of call to made
/// * `format` - Format of the data pushed onto the bus
/// * `buf` - The buffer where data to be transmitted is stored
pub fn bus_subcall<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    parent: __wasi_cid_t,
    topic_hash: WasmPtr<__wasi_hash_t>,
    format: __wasi_busdataformat_t,
    buf: WasmPtr<u8, M>,
    buf_len: M::Offset,
    ret_cid: WasmPtr<__wasi_cid_t, M>,
) -> Result<__bus_errno_t, WasiError> {
    let env = ctx.data();
    let bus = env.runtime.bus();
    let memory = env.memory_view(&ctx);
    let topic_hash = wasi_try_mem_bus_ok!(topic_hash.read(&memory));
    let buf_slice = wasi_try_mem_bus_ok!(buf.slice(&memory, buf_len));
    trace!(
        "wasi::bus_subcall (parent={}, buf_len={})",
        parent,
        buf_len
    );

    let format = wasi_try_bus_ok!(conv_bus_format_from(format));
    let buf = wasi_try_mem_bus_ok!(buf_slice.read_to_vec());

    // Get the parent call that we'll invoke this call for
    let mut guard = env.state.bus.protected();
    if let Some(parent) = guard.calls.get(&parent)
    {
        let bid = parent.bid.clone();

        // Invoke the sub-call in the existing parent call
        let mut invoked = parent.invocation.invoke(topic_hash, format, buf);
        drop(parent);
        drop(guard);

        // Poll the invocation until it does its thing
        let invocation;
        {
            // Fast path (does not have to create a futex creation)
            let waker = WasiDummyWaker.into_waker();
            let mut cx = Context::from_waker(&waker);
            let pinned_invoked = Pin::new(invoked.deref_mut());
            match pinned_invoked.poll_invoked(&mut cx) {
                Poll::Ready(i) => {
                    invocation = wasi_try_bus_ok!(i
                        .map_err(bus_error_into_wasi_err));
                },
                Poll::Pending => {
                    // Slow path (will put the thread to sleep)
                    let parking = WasiParkingLot::default();
                    let waker = parking.get_waker();
                    let mut cx = Context::from_waker(&waker);
                    loop {
                        let pinned_invoked = Pin::new(invoked.deref_mut());
                        match pinned_invoked.poll_invoked(&mut cx) {
                            Poll::Ready(i) => {
                                invocation = wasi_try_bus_ok!(i
                                    .map_err(bus_error_into_wasi_err));
                                break;
                            },
                            Poll::Pending => {
                                env.yield_now()?;
                                parking.wait(Duration::from_millis(5));
                            }
                        }
                    }
                }
            }
        }
        
        // Add the call and return the ID
        let cid = {
            let mut guard = env.state.bus.protected();
            guard.call_seed += 1;
            let cid = guard.call_seed;
            guard.calls.insert(cid, WasiBusCall {
                bid,
                invocation
            });
            cid
        };

        // Now we wake any BUS pollers so that they can drive forward the
        // call to completion - when they poll the call they will also
        // register a BUS waker
        env.state.bus.poll_wake();

        // Return the CID and success to the caller
        wasi_try_mem_bus_ok!(ret_cid.write(&memory, cid));
        Ok(__BUS_ESUCCESS)
    } else {
        Ok(__BUS_EBADHANDLE)
    }
}

// Function for converting the format
fn conv_bus_format(format: BusDataFormat) -> __wasi_busdataformat_t {
    match format {
        BusDataFormat::Raw => __WASI_BUS_DATA_FORMAT_RAW,
        BusDataFormat::Bincode => __WASI_BUS_DATA_FORMAT_BINCODE,
        BusDataFormat::MessagePack => __WASI_BUS_DATA_FORMAT_MESSAGE_PACK,
        BusDataFormat::Json => __WASI_BUS_DATA_FORMAT_JSON,
        BusDataFormat::Yaml => __WASI_BUS_DATA_FORMAT_YAML,
        BusDataFormat::Xml => __WASI_BUS_DATA_FORMAT_XML,
    }
}

fn conv_bus_format_from(format: __wasi_busdataformat_t) -> Result<BusDataFormat, __bus_errno_t> {
    Ok(
        match format {
            __WASI_BUS_DATA_FORMAT_RAW => BusDataFormat::Raw,
            __WASI_BUS_DATA_FORMAT_BINCODE => BusDataFormat::Bincode,
            __WASI_BUS_DATA_FORMAT_MESSAGE_PACK => BusDataFormat::MessagePack,
            __WASI_BUS_DATA_FORMAT_JSON => BusDataFormat::Json,
            __WASI_BUS_DATA_FORMAT_YAML => BusDataFormat::Yaml,
            __WASI_BUS_DATA_FORMAT_XML => BusDataFormat::Xml,
            _ => { return Err(__BUS_EDES); }
        }
    )
}

/// Polls for any outstanding events from a particular
/// bus process by its handle
///
/// ## Parameters
///
/// * `timeout` - Timeout before the poll returns, if one passed 0
///   as the timeout then this call is non blocking.
/// * `events` - An events buffer that will hold any received bus events
/// * `malloc` - Name of the function that will be invoked to allocate memory
///   Function signature fn(u64) -> u64
///
/// ## Return
///
/// Returns the number of events that have occured
pub fn bus_poll<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    timeout: __wasi_timestamp_t,
    events: WasmPtr<__wasi_busevent_t, M>,
    maxevents: M::Offset,
    ret_nevents: WasmPtr<M::Offset, M>,
) -> Result<__bus_errno_t, WasiError> {
    let env = ctx.data();
    let bus = env.runtime.bus();
    let memory = env.memory_view(&ctx);
    trace!("wasi::bus_poll (timeout={})", timeout);

    // Lets start by processing events for calls that are already running
    let mut nevents = M::ZERO;
    let events = wasi_try_mem_bus_ok!(events.slice(&memory, maxevents));
    
    let state = env.state.clone();
    let start = platform_clock_time_get(__WASI_CLOCK_MONOTONIC, 1_000_000).unwrap() as u128;
    loop
    {
        // The waker will wake this thread should any work arrive
        // or need further processing (i.e. async operation)
        let waker = state.bus.get_poll_waker();
        let mut cx = Context::from_waker(&waker);

        // Check if any of the processes have closed
        let mut exited_bids = HashSet::new();
        {
            let mut guard = env.state.threading.write().unwrap();
            for (pid, process) in guard.bus_processes.iter_mut() {
                let pinned_process = Pin::new(process.inst.as_mut());
                if pinned_process.poll_finished(&mut cx) == Poll::Ready(()) {
                    exited_bids.insert(*pid);
                }
            }
            for pid in exited_bids.iter() {
                guard.bus_processes.remove(pid);
            }
        }

        {
            // The waker will trigger the reactors when work arrives from the BUS
            let mut guard = env.state.bus.protected();
            
            // Function that hashes the topic using SHA256
            let hash_topic = |topic: Cow<'static, str>| -> __wasi_hash_t {
                use sha2::{Sha256, Digest};
                let mut hasher = Sha256::new();
                hasher.update(&topic.bytes().collect::<Vec<_>>());
                let hash: [u8; 16] = hasher.finalize()[..16].try_into().unwrap();
                u128::from_le_bytes(hash)
            };

            // Function that turns a buffer into a readable file handle
            let buf_to_fd = {
                let state = env.state.clone();
                let inodes = state.inodes.clone();
                move |data: Vec<u8>| -> __wasi_fd_t {
                    let mut inodes = inodes.write().unwrap();
                    let inode = state.fs.create_inode_with_default_stat(
                        inodes.deref_mut(),
                        Kind::Buffer { buffer: data },
                        false,
                        "bus".into(),
                    );
                    let rights = super::state::bus_read_rights();
                    wasi_try_bus!(state.fs.create_fd(rights, rights, 0, 0, inode)
                        .map_err(|err| {
                            debug!("failed to create file descriptor for BUS event buffer - {}", err);
                            __BUS_EALLOC
                        }))
                }
            };
            
            // Grab all the events we can from all the existing calls up to the limit of
            // maximum events that the user requested
            if nevents < maxevents {
                let mut drop_calls = Vec::new();
                let mut call_seed = guard.call_seed;
                for (key, call) in guard.calls.iter_mut() {
                    let cid: __wasi_cid_t = (*key).into();

                    if nevents >= maxevents {
                        break;
                    }

                    // If the process that is hosting the call is finished then so is the call
                    if exited_bids.contains(&call.bid) {
                        drop_calls.push(*key);
                        trace!("wasi::bus_poll (aborted, cid={})", cid);                        
                        let evt = unsafe {
                            std::mem::transmute(__wasi_busevent_t2 {
                                tag: __WASI_BUS_EVENT_TYPE_FAULT,
                                u: __wasi_busevent_u {
                                    fault: __wasi_busevent_fault_t {
                                        cid,
                                        err: __BUS_EABORTED
                                    }
                                }
                            })
                        };
                
                        let nevents64: u64 = wasi_try_bus_ok!(nevents.try_into().map_err(|_| __BUS_EINTERNAL));
                        wasi_try_mem_bus_ok!(events.write(nevents64, evt));

                        nevents += M::ONE;
                        continue;
                    }

                    // Otherwise lets poll for events
                    while nevents < maxevents {
                        let mut finished = false;
                        let call = Pin::new(call.invocation.as_mut());
                        match call.poll_event(&mut cx) {
                            Poll::Ready(evt) =>
                            {
                                let evt = match evt {
                                    BusInvocationEvent::Callback { topic_hash, format, data } => {
                                        let sub_cid = {
                                            call_seed += 1;
                                            call_seed
                                        };
                        
                                        trace!("wasi::bus_poll (callback, parent={}, cid={}, topic={})", cid, sub_cid, topic_hash);
                                        __wasi_busevent_t2 {
                                            tag: __WASI_BUS_EVENT_TYPE_CALL,
                                            u: __wasi_busevent_u {
                                                call: __wasi_busevent_call_t {
                                                    parent: __wasi_option_cid_t {
                                                        tag: __WASI_OPTION_SOME,
                                                        cid,
                                                    },
                                                    cid: sub_cid,
                                                    format: conv_bus_format(format),
                                                    topic_hash,
                                                    fd: buf_to_fd(data), 
                                                }
                                            }
                                        }
                                    },
                                    BusInvocationEvent::Response { format, data } => {
                                        drop_calls.push(*key);
                                        finished = true;

                                        trace!("wasi::bus_poll (response, cid={}, len={})", cid, data.len());
                                        __wasi_busevent_t2 {
                                            tag: __WASI_BUS_EVENT_TYPE_RESULT,
                                            u: __wasi_busevent_u {
                                                result: __wasi_busevent_result_t {
                                                    format: conv_bus_format(format),
                                                    cid,
                                                    fd: buf_to_fd(data),
                                                }
                                            }
                                        }
                                    },
                                    BusInvocationEvent::Fault { fault } => {
                                        drop_calls.push(*key);
                                        finished = true;

                                        trace!("wasi::bus_poll (fault, cid={}, err={})", cid, fault);
                                        __wasi_busevent_t2 {
                                            tag: __WASI_BUS_EVENT_TYPE_FAULT,
                                            u: __wasi_busevent_u {
                                                fault: __wasi_busevent_fault_t {
                                                    cid,
                                                    err: bus_error_into_wasi_err(fault)
                                                }
                                            }
                                        }
                                    }
                                };
                                let evt = unsafe {
                                    std::mem::transmute(evt)
                                };
                        
                                let nevents64: u64 = wasi_try_bus_ok!(nevents.try_into().map_err(|_| __BUS_EINTERNAL));
                                wasi_try_mem_bus_ok!(events.write(nevents64, evt));

                                nevents += M::ONE;

                                if finished {
                                    break;
                                }
                            },
                            Poll::Pending => { break; }
                        }
                    }
                }
                guard.call_seed = call_seed;

                // Drop any calls that are no longer in scope
                if drop_calls.is_empty() == false {
                    for key in drop_calls {
                        guard.calls.remove(&key);
                    }
                }
            }

            if nevents < maxevents {
                let mut call_seed = guard.call_seed;
                let mut to_add = Vec::new();
                for (key, call) in guard.called.iter_mut() {
                    let cid: __wasi_cid_t = (*key).into();
                    while nevents < maxevents {
                        let call = Pin::new(call.deref_mut());
                        match call.poll(&mut cx) {
                            Poll::Ready(event) => {
                                // Register the call
                                let sub_cid = {
                                    call_seed += 1;
                                    to_add.push((call_seed, event.called));
                                    call_seed
                                };

                                let event = __wasi_busevent_t2 {
                                    tag: __WASI_BUS_EVENT_TYPE_CALL,
                                    u: __wasi_busevent_u {
                                        call: __wasi_busevent_call_t {
                                            parent: __wasi_option_cid_t {
                                                tag: __WASI_OPTION_SOME,
                                                cid,
                                            },
                                            cid: sub_cid,
                                            format: conv_bus_format(event.format),
                                            topic_hash: event.topic_hash,
                                            fd: buf_to_fd(event.data),
                                        }
                                    }
                                };
                                let event = unsafe {
                                    std::mem::transmute(event)
                                };
                                
                                let nevents64: u64 = wasi_try_bus_ok!(nevents.try_into().map_err(|_| __BUS_EINTERNAL));
                                wasi_try_mem_bus_ok!(events.write(nevents64, event));
                                nevents += M::ONE;
                            },
                            Poll::Pending => { break; }
                        };
                    }
                    if nevents >= maxevents {
                        break;
                    }
                }

                guard.call_seed = call_seed;
                for (cid, called) in to_add {
                    guard.called.insert(cid, called);
                }
            }

            while nevents < maxevents
            {
                // Check the listener (if none exists then one is created)
                let event = {
                    let listener = wasi_try_bus_ok!(env.runtime
                        .bus()
                        .listen()
                        .map_err(bus_error_into_wasi_err));
                    let listener = Pin::new(listener.deref());
                    listener.poll(&mut cx)
                };

                // Process the event returned by the listener or exit the poll loop
                let event = match event {
                    Poll::Ready(event) => {

                        // Register the call
                        let sub_cid = {
                            guard.call_seed += 1;
                            let cid = guard.call_seed;
                            guard.called.insert(cid, event.called);
                            cid
                        };

                        __wasi_busevent_t2 {
                            tag: __WASI_BUS_EVENT_TYPE_CALL,
                            u: __wasi_busevent_u {
                                call: __wasi_busevent_call_t {
                                    parent: __wasi_option_cid_t {
                                        tag: __WASI_OPTION_NONE,
                                        cid: 0,
                                    },
                                    cid: sub_cid,
                                    format: conv_bus_format(event.format),
                                    topic_hash: event.topic_hash,
                                    fd: buf_to_fd(event.data),
                                }
                            }
                        }
                    },
                    Poll::Pending => { break; }
                };
                let event = unsafe {
                    std::mem::transmute(event)
                };
                
                let nevents64: u64 = wasi_try_bus_ok!(nevents.try_into().map_err(|_| __BUS_EINTERNAL));
                wasi_try_mem_bus_ok!(events.write(nevents64, event));
                nevents += M::ONE;
            }
        }

        // If we still have no events
        if nevents >= M::ONE {
            break;
        }
        
        // Every 100 milliseconds we check if the thread needs to terminate (via `env.yield_now`)
        // otherwise the loop will break if the BUS futex is triggered or a timeout is reached
        loop {
            // Check for timeout (zero will mean the loop will not wait)
            let now = platform_clock_time_get(__WASI_CLOCK_MONOTONIC, 1_000_000).unwrap() as u128;
            let delta = now.checked_sub(start).unwrap_or(0) as __wasi_timestamp_t;
            if delta >= timeout {
                trace!("wasi::bus_poll (timeout)");
                wasi_try_mem_bus_ok!(ret_nevents.write(&memory, nevents));
                return Ok(__BUS_ESUCCESS);
            }

            env.yield_now()?;

            let remaining = timeout.checked_sub(delta).unwrap_or(0);
            let interval = Duration::from_nanos(remaining)
                .min(Duration::from_millis(5)) // we don't want the CPU burning
                .max(Duration::from_millis(100)); // 100 milliseconds to kill worker threads seems acceptable
            if state.bus.poll_wait(interval) == true {
                break;
            }
        }
    }
    if nevents > M::ZERO {
        trace!("wasi::bus_poll (return nevents={})", nevents);
    } else {
        trace!("wasi::bus_poll (idle - no events)");
    }

    wasi_try_mem_bus_ok!(ret_nevents.write(&memory, nevents));
    Ok(__BUS_ESUCCESS)
}

/// Replies to a call that was made to this process
/// from another process; where 'cid' is the call context.
/// This will may also drop the handle and release any
/// associated resources (if keepalive is not set)
///
/// ## Parameters
///
/// * `cid` - Handle of the call to send a reply on
/// * `format` - Format of the data pushed onto the bus
/// * `buf` - The buffer where data to be transmitted is stored
pub fn call_reply<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    cid: __wasi_cid_t,
    format: __wasi_busdataformat_t,
    buf: WasmPtr<u8, M>,
    buf_len: M::Offset,
) -> __bus_errno_t {
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let bus = env.runtime.bus();
    trace!(
        "wasi::call_reply (cid={}, format={}, data_len={})",
        cid,
        format,
        buf_len
    );
    let buf_slice = wasi_try_mem_bus!(buf.slice(&memory, buf_len));
    let buf = wasi_try_mem_bus!(buf_slice.read_to_vec());

    let mut guard = env.state.bus.protected();
    if let Some(call) = guard.called.remove(&cid) {
        drop(guard);

        let format = wasi_try_bus!(conv_bus_format_from(format));
        call.reply(format, buf);
        __BUS_ESUCCESS
    } else {
        __BUS_EBADHANDLE
    }
}

/// Causes a fault on a particular call that was made
/// to this process from another process; where 'bid'
/// is the callering process context.
///
/// ## Parameters
///
/// * `cid` - Handle of the call to raise a fault on
/// * `fault` - Fault to be raised on the bus
pub fn call_fault(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    cid: __wasi_cid_t,
    fault: __bus_errno_t)
{
    let env = ctx.data();
    let bus = env.runtime.bus();
    debug!("wasi::call_fault (cid={}, fault={})", cid, fault);

    let mut guard = env.state.bus.protected();
    guard.calls.remove(&cid);

    if let Some(call) = guard.called.remove(&cid) {
        drop(guard);
        call.fault(wasi_error_into_bus_err(fault));
    }
}

/// Closes a bus call based on its bus call handle
///
/// ## Parameters
///
/// * `cid` - Handle of the bus call handle to be dropped
pub fn call_close(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    cid: __wasi_cid_t
) {
    let env = ctx.data();
    let bus = env.runtime.bus();
    trace!("wasi::call_close (cid={})", cid);

    let mut guard = env.state.bus.protected();
    guard.calls.remove(&cid);
    guard.called.remove(&cid);
}

/// ### `ws_connect()`
/// Connects to a websocket at a particular network URL
///
/// ## Parameters
///
/// * `url` - URL of the web socket destination to connect to
///
/// ## Return
///
/// Returns a socket handle which is used to send and receive data
pub fn ws_connect<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    url: WasmPtr<u8, M>,
    url_len: M::Offset,
    ret_sock: WasmPtr<__wasi_fd_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::ws_connect");
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let url = unsafe { get_input_str!(&memory, url, url_len) };

    let socket = wasi_try!(env
        .net()
        .ws_connect(url.as_str())
        .map_err(net_error_into_wasi_err));

    let (memory, state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

    let kind = Kind::Socket {
        socket: InodeSocket::new(InodeSocketKind::WebSocket(socket)),
    };

    let inode = state.fs.create_inode_with_default_stat(
        inodes.deref_mut(),
        kind,
        false,
        "socket".into(),
    );
    let rights = super::state::all_socket_rights();
    let fd = wasi_try!(state.fs.create_fd(rights, rights, 0, 0, inode));

    wasi_try_mem!(ret_sock.write(&memory, fd));

    __WASI_ESUCCESS
}

/// ### `http_request()`
/// Makes a HTTP request to a remote web resource and
/// returns a socket handles that are used to send and receive data
///
/// ## Parameters
///
/// * `url` - URL of the HTTP resource to connect to
/// * `method` - HTTP method to be invoked
/// * `headers` - HTTP headers to attach to the request
///   (headers seperated by lines)
/// * `gzip` - Should the request body be compressed
///
/// ## Return
///
/// The body of the response can be streamed from the returned
/// file handle
pub fn http_request<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    url: WasmPtr<u8, M>,
    url_len: M::Offset,
    method: WasmPtr<u8, M>,
    method_len: M::Offset,
    headers: WasmPtr<u8, M>,
    headers_len: M::Offset,
    gzip: __wasi_bool_t,
    ret_handles: WasmPtr<__wasi_http_handles_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::http_request");
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let url = unsafe { get_input_str!(&memory, url, url_len) };
    let method = unsafe { get_input_str!(&memory, method, method_len) };
    let headers = unsafe { get_input_str!(&memory, headers, headers_len) };

    let gzip = match gzip {
        __WASI_BOOL_FALSE => false,
        __WASI_BOOL_TRUE => true,
        _ => return __WASI_EINVAL,
    };

    let socket = wasi_try!(env
        .net()
        .http_request(url.as_str(), method.as_str(), headers.as_str(), gzip)
        .map_err(net_error_into_wasi_err));
    let socket_req = SocketHttpRequest {
        request: socket.request,
        response: None,
        headers: None,
        status: socket.status.clone(),
    };
    let socket_res = SocketHttpRequest {
        request: None,
        response: socket.response,
        headers: None,
        status: socket.status.clone(),
    };
    let socket_hdr = SocketHttpRequest {
        request: None,
        response: None,
        headers: socket.headers,
        status: socket.status,
    };

    let (memory, state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

    let kind_req = Kind::Socket {
        socket: InodeSocket::new(InodeSocketKind::HttpRequest(
            Mutex::new(socket_req),
            InodeHttpSocketType::Request,
        )),
    };
    let kind_res = Kind::Socket {
        socket: InodeSocket::new(InodeSocketKind::HttpRequest(
            Mutex::new(socket_res),
            InodeHttpSocketType::Response,
        )),
    };
    let kind_hdr = Kind::Socket {
        socket: InodeSocket::new(InodeSocketKind::HttpRequest(
            Mutex::new(socket_hdr),
            InodeHttpSocketType::Headers,
        )),
    };

    let inode_req = state.fs.create_inode_with_default_stat(
        inodes.deref_mut(),
        kind_req,
        false,
        "http_request".into(),
    );
    let inode_res = state.fs.create_inode_with_default_stat(
        inodes.deref_mut(),
        kind_res,
        false,
        "http_response".into(),
    );
    let inode_hdr = state.fs.create_inode_with_default_stat(
        inodes.deref_mut(),
        kind_hdr,
        false,
        "http_headers".into(),
    );
    let rights = super::state::all_socket_rights();

    let handles = __wasi_http_handles_t {
        req: wasi_try!(state.fs.create_fd(rights, rights, 0, 0, inode_req)),
        res: wasi_try!(state.fs.create_fd(rights, rights, 0, 0, inode_res)),
        hdr: wasi_try!(state.fs.create_fd(rights, rights, 0, 0, inode_hdr)),
    };

    wasi_try_mem!(ret_handles.write(&memory, handles));

    __WASI_ESUCCESS
}

/// ### `http_status()`
/// Retrieves the status of a HTTP request
///
/// ## Parameters
///
/// * `fd` - Handle of the HTTP request
/// * `status` - Pointer to a buffer that will be filled with the current
///   status of this HTTP request
pub fn http_status<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    status: WasmPtr<__wasi_http_status_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::http_status");

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let ref_status = status.deref(&memory);

    let http_status = wasi_try!(__sock_actor(&ctx, sock, 0, |socket| {
        socket.http_status()
    }));

    // Write everything else and return the status to the caller
    let status = __wasi_http_status_t {
        ok: __WASI_BOOL_TRUE,
        redirect: match http_status.redirected {
            true => __WASI_BOOL_TRUE,
            false => __WASI_BOOL_FALSE,
        },
        size: wasi_try!(Ok(http_status.size)),
        status: http_status.status,
    };

    wasi_try_mem!(ref_status.write(status));

    __WASI_ESUCCESS
}

/// ### `port_bridge()`
/// Securely connects to a particular remote network
///
/// ## Parameters
///
/// * `network` - Fully qualified identifier for the network
/// * `token` - Access token used to authenticate with the network
/// * `security` - Level of encryption to encapsulate the network connection with
pub fn port_bridge<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    network: WasmPtr<u8, M>,
    network_len: M::Offset,
    token: WasmPtr<u8, M>,
    token_len: M::Offset,
    security: __wasi_streamsecurity_t,
) -> __wasi_errno_t {
    debug!("wasi::port_bridge");
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let network = unsafe { get_input_str!(&memory, network, network_len) };
    let token = unsafe { get_input_str!(&memory, token, token_len) };
    let security = match security {
        __WASI_STREAM_SECURITY_UNENCRYPTED => StreamSecurity::Unencrypted,
        __WASI_STREAM_SECURITY_ANY_ENCRYPTION => StreamSecurity::AnyEncyption,
        __WASI_STREAM_SECURITY_CLASSIC_ENCRYPTION => StreamSecurity::ClassicEncryption,
        __WASI_STREAM_SECURITY_DOUBLE_ENCRYPTION => StreamSecurity::DoubleEncryption,
        _ => return __WASI_EINVAL,
    };

    wasi_try!(env
        .net()
        .bridge(network.as_str(), token.as_str(), security)
        .map_err(net_error_into_wasi_err));
    __WASI_ESUCCESS
}

/// ### `port_unbridge()`
/// Disconnects from a remote network
pub fn port_unbridge(ctx: FunctionEnvMut<'_, WasiEnv>) -> __wasi_errno_t {
    debug!("wasi::port_unbridge");
    let env = ctx.data();
    wasi_try!(env.net().unbridge().map_err(net_error_into_wasi_err));
    __WASI_ESUCCESS
}

/// ### `port_dhcp_acquire()`
/// Acquires a set of IP addresses using DHCP
pub fn port_dhcp_acquire(ctx: FunctionEnvMut<'_, WasiEnv>) -> __wasi_errno_t {
    debug!("wasi::port_dhcp_acquire");
    let env = ctx.data();
    wasi_try!(env.net().dhcp_acquire().map_err(net_error_into_wasi_err));
    __WASI_ESUCCESS
}

/// ### `port_addr_add()`
/// Adds another static address to the local port
///
/// ## Parameters
///
/// * `addr` - Address to be added
pub fn port_addr_add<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    ip: WasmPtr<__wasi_cidr_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::port_addr_add");
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let cidr = wasi_try!(super::state::read_cidr(&memory, ip));
    wasi_try!(env
        .net()
        .ip_add(cidr.ip, cidr.prefix)
        .map_err(net_error_into_wasi_err));
    __WASI_ESUCCESS
}

/// ### `port_addr_remove()`
/// Removes an address from the local port
///
/// ## Parameters
///
/// * `addr` - Address to be removed
pub fn port_addr_remove<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    ip: WasmPtr<__wasi_addr_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::port_addr_remove");
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let ip = wasi_try!(super::state::read_ip(&memory, ip));
    wasi_try!(env.net().ip_remove(ip).map_err(net_error_into_wasi_err));
    __WASI_ESUCCESS
}

/// ### `port_addr_clear()`
/// Clears all the addresses on the local port
pub fn port_addr_clear(ctx: FunctionEnvMut<'_, WasiEnv>) -> __wasi_errno_t {
    debug!("wasi::port_addr_clear");
    let env = ctx.data();
    wasi_try!(env.net().ip_clear().map_err(net_error_into_wasi_err));
    __WASI_ESUCCESS
}

/// ### `port_mac()`
/// Returns the MAC address of the local port
pub fn port_mac<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    ret_mac: WasmPtr<__wasi_hardwareaddress_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::port_mac");
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let mac = wasi_try!(env.net().mac().map_err(net_error_into_wasi_err));
    let mac = __wasi_hardwareaddress_t { octs: mac };
    wasi_try_mem!(ret_mac.write(&memory, mac));
    __WASI_ESUCCESS
}

/// ### `port_ip_list()`
/// Returns a list of all the addresses owned by the local port
/// This function fills the output buffer as much as possible.
/// If the buffer is not big enough then the naddrs address will be
/// filled with the buffer size needed and the EOVERFLOW will be returned
///
/// ## Parameters
///
/// * `addrs` - The buffer where addresses will be stored
///
/// ## Return
///
/// The number of addresses returned.
pub fn port_addr_list<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    addrs: WasmPtr<__wasi_cidr_t, M>,
    naddrs: WasmPtr<M::Offset, M>,
) -> __wasi_errno_t {
    debug!("wasi::port_addr_list");
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let max_addrs = wasi_try_mem!(naddrs.read(&memory));
    let max_addrs: u64 = wasi_try!(max_addrs.try_into().map_err(|_| __WASI_EOVERFLOW));
    let ref_addrs =
        wasi_try_mem!(addrs.slice(&memory, wasi_try!(to_offset::<M>(max_addrs as usize))));

    let addrs = wasi_try!(env.net().ip_list().map_err(net_error_into_wasi_err));

    let addrs_len: M::Offset = wasi_try!(addrs.len().try_into().map_err(|_| __WASI_EOVERFLOW));
    wasi_try_mem!(naddrs.write(&memory, addrs_len));
    if addrs.len() as u64 > max_addrs {
        return __WASI_EOVERFLOW;
    }

    for n in 0..addrs.len() {
        let nip = ref_addrs.index(n as u64);
        super::state::write_cidr(&memory, nip.as_ptr::<M>(), *addrs.get(n).unwrap());
    }

    __WASI_ESUCCESS
}

/// ### `port_gateway_set()`
/// Adds a default gateway to the port
///
/// ## Parameters
///
/// * `addr` - Address of the default gateway
pub fn port_gateway_set<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    ip: WasmPtr<__wasi_addr_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::port_gateway_set");
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let ip = wasi_try!(super::state::read_ip(&memory, ip));

    wasi_try!(env.net().gateway_set(ip).map_err(net_error_into_wasi_err));
    __WASI_ESUCCESS
}

/// ### `port_route_add()`
/// Adds a new route to the local port
pub fn port_route_add<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    cidr: WasmPtr<__wasi_cidr_t, M>,
    via_router: WasmPtr<__wasi_addr_t, M>,
    preferred_until: WasmPtr<__wasi_option_timestamp_t, M>,
    expires_at: WasmPtr<__wasi_option_timestamp_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::port_route_add");
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let cidr = wasi_try!(super::state::read_cidr(&memory, cidr));
    let via_router = wasi_try!(super::state::read_ip(&memory, via_router));
    let preferred_until = wasi_try_mem!(preferred_until.read(&memory));
    let preferred_until = match preferred_until.tag {
        __WASI_OPTION_NONE => None,
        __WASI_OPTION_SOME => Some(Duration::from_nanos(preferred_until.u)),
        _ => return __WASI_EINVAL,
    };
    let expires_at = wasi_try_mem!(expires_at.read(&memory));
    let expires_at = match expires_at.tag {
        __WASI_OPTION_NONE => None,
        __WASI_OPTION_SOME => Some(Duration::from_nanos(expires_at.u)),
        _ => return __WASI_EINVAL,
    };

    wasi_try!(env
        .net()
        .route_add(cidr, via_router, preferred_until, expires_at)
        .map_err(net_error_into_wasi_err));
    __WASI_ESUCCESS
}

/// ### `port_route_remove()`
/// Removes an existing route from the local port
pub fn port_route_remove<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    ip: WasmPtr<__wasi_addr_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::port_route_remove");
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let ip = wasi_try!(super::state::read_ip(&memory, ip));
    wasi_try!(env.net().route_remove(ip).map_err(net_error_into_wasi_err));
    __WASI_ESUCCESS
}

/// ### `port_route_clear()`
/// Clears all the routes in the local port
pub fn port_route_clear(ctx: FunctionEnvMut<'_, WasiEnv>) -> __wasi_errno_t {
    debug!("wasi::port_route_clear");
    let env = ctx.data();
    wasi_try!(env.net().route_clear().map_err(net_error_into_wasi_err));
    __WASI_ESUCCESS
}

/// ### `port_route_list()`
/// Returns a list of all the routes owned by the local port
/// This function fills the output buffer as much as possible.
/// If the buffer is too small this will return EOVERFLOW and
/// fill nroutes with the size of the buffer needed.
///
/// ## Parameters
///
/// * `routes` - The buffer where routes will be stored
pub fn port_route_list<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    routes: WasmPtr<__wasi_route_t, M>,
    nroutes: WasmPtr<M::Offset, M>,
) -> __wasi_errno_t {
    debug!("wasi::port_route_list");
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let nroutes = nroutes.deref(&memory);
    let max_routes: usize = wasi_try!(wasi_try_mem!(nroutes.read())
        .try_into()
        .map_err(|_| __WASI_EINVAL));
    let ref_routes =
        wasi_try_mem!(routes.slice(&memory, wasi_try!(to_offset::<M>(max_routes))));

    let routes = wasi_try!(env.net().route_list().map_err(net_error_into_wasi_err));

    let routes_len: M::Offset = wasi_try!(routes.len().try_into().map_err(|_| __WASI_EINVAL));
    wasi_try_mem!(nroutes.write(routes_len));
    if routes.len() > max_routes {
        return __WASI_EOVERFLOW;
    }

    for n in 0..routes.len() {
        let nroute = ref_routes.index(n as u64);
        super::state::write_route(
            &memory,
            nroute.as_ptr::<M>(),
            routes.get(n).unwrap().clone(),
        );
    }

    __WASI_ESUCCESS
}

/// ### `sock_shutdown()`
/// Shut down socket send and receive channels.
/// Note: This is similar to `shutdown` in POSIX.
///
/// ## Parameters
///
/// * `how` - Which channels on the socket to shut down.
pub fn sock_shutdown(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    how: __wasi_sdflags_t,
) -> __wasi_errno_t {
    debug!("wasi::sock_shutdown");

    let both = __WASI_SHUT_RD | __WASI_SHUT_WR;
    let how = match how {
        __WASI_SHUT_RD => std::net::Shutdown::Read,
        __WASI_SHUT_WR => std::net::Shutdown::Write,
        a if a == both => std::net::Shutdown::Both,
        _ => return __WASI_EINVAL,
    };

    wasi_try!(__sock_actor_mut(
        &ctx,
        sock,
        __WASI_RIGHT_SOCK_SHUTDOWN,
        |socket| { socket.shutdown(how) }
    ));

    __WASI_ESUCCESS
}

/// ### `sock_status()`
/// Returns the current status of a socket
pub fn sock_status<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    ret_status: WasmPtr<__wasi_sockstatus_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::sock_status");

    let status = wasi_try!(__sock_actor(&ctx, sock, 0, |socket| { socket.status() }));

    use super::state::WasiSocketStatus;
    let status = match status {
        WasiSocketStatus::Opening => __WASI_SOCK_STATUS_OPENING,
        WasiSocketStatus::Opened => __WASI_SOCK_STATUS_OPENED,
        WasiSocketStatus::Closed => __WASI_SOCK_STATUS_CLOSED,
        WasiSocketStatus::Failed => __WASI_SOCK_STATUS_FAILED,
    };

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    wasi_try_mem!(ret_status.write(&memory, status));

    __WASI_ESUCCESS
}

/// ### `sock_addr_local()`
/// Returns the local address to which the socket is bound.
///
/// Note: This is similar to `getsockname` in POSIX
///
/// When successful, the contents of the output buffer consist of an IP address,
/// either IP4 or IP6.
///
/// ## Parameters
///
/// * `fd` - Socket that the address is bound to
pub fn sock_addr_local<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    ret_addr: WasmPtr<__wasi_addr_port_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::sock_addr_local");

    let addr = wasi_try!(__sock_actor(&ctx, sock, 0, |socket| {
        socket.addr_local()
    }));
    let memory = ctx.data().memory_view(&ctx);
    wasi_try!(super::state::write_ip_port(
        &memory,
        ret_addr,
        addr.ip(),
        addr.port()
    ));
    __WASI_ESUCCESS
}

/// ### `sock_addr_peer()`
/// Returns the remote address to which the socket is connected to.
///
/// Note: This is similar to `getpeername` in POSIX
///
/// When successful, the contents of the output buffer consist of an IP address,
/// either IP4 or IP6.
///
/// ## Parameters
///
/// * `fd` - Socket that the address is bound to
pub fn sock_addr_peer<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    ro_addr: WasmPtr<__wasi_addr_port_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::sock_addr_peer");

    let env = ctx.data();
    let addr = wasi_try!(__sock_actor(&ctx, sock, 0, |socket| { socket.addr_peer() }));
    let memory = env.memory_view(&ctx);
    wasi_try!(super::state::write_ip_port(
        &memory,
        ro_addr,
        addr.ip(),
        addr.port()
    ));
    __WASI_ESUCCESS
}

/// ### `sock_open()`
/// Create an endpoint for communication.
///
/// creates an endpoint for communication and returns a file descriptor
/// tor that refers to that endpoint. The file descriptor returned by a successful
/// call will be the lowest-numbered file descriptor not currently open
/// for the process.
///
/// Note: This is similar to `socket` in POSIX using PF_INET
///
/// ## Parameters
///
/// * `af` - Address family
/// * `socktype` - Socket type, either datagram or stream
/// * `sock_proto` - Socket protocol
///
/// ## Return
///
/// The file descriptor of the socket that has been opened.
pub fn sock_open<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    af: __wasi_addressfamily_t,
    ty: __wasi_socktype_t,
    pt: __wasi_sockproto_t,
    ro_sock: WasmPtr<__wasi_fd_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::sock_open");

    let env = ctx.data();
    let (memory, state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

    let kind = match ty {
        __WASI_SOCK_TYPE_STREAM | __WASI_SOCK_TYPE_DGRAM => Kind::Socket {
            socket: InodeSocket::new(InodeSocketKind::PreSocket {
                family: af,
                ty,
                pt,
                addr: None,
                only_v6: false,
                reuse_port: false,
                reuse_addr: false,
                send_buf_size: None,
                recv_buf_size: None,
                send_timeout: None,
                recv_timeout: None,
                connect_timeout: None,
                accept_timeout: None,
            }),
        },
        _ => return __WASI_ENOTSUP,
    };

    let inode = state.fs.create_inode_with_default_stat(
        inodes.deref_mut(),
        kind,
        false,
        "socket".into(),
    );
    let rights = super::state::all_socket_rights();
    let fd = wasi_try!(state.fs.create_fd(rights, rights, 0, 0, inode));

    wasi_try_mem!(ro_sock.write(&memory, fd));

    __WASI_ESUCCESS
}

/// ### `sock_set_opt_flag()`
/// Sets a particular socket setting
/// Note: This is similar to `setsockopt` in POSIX for SO_REUSEADDR
///
/// ## Parameters
///
/// * `fd` - Socket descriptor
/// * `sockopt` - Socket option to be set
/// * `flag` - Value to set the option to
pub fn sock_set_opt_flag(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    opt: __wasi_sockoption_t,
    flag: __wasi_bool_t,
) -> __wasi_errno_t {
    debug!("wasi::sock_set_opt_flag(ty={})", opt);

    let flag = match flag {
        __WASI_BOOL_FALSE => false,
        __WASI_BOOL_TRUE => true,
        _ => return __WASI_EINVAL,
    };

    let option: super::state::WasiSocketOption = opt.into();
    wasi_try!(__sock_actor_mut(&ctx, sock, 0, |socket| {
        socket.set_opt_flag(option, flag)
    }));
    __WASI_ESUCCESS
}

/// ### `sock_get_opt_flag()`
/// Retrieve status of particular socket seting
/// Note: This is similar to `getsockopt` in POSIX for SO_REUSEADDR
///
/// ## Parameters
///
/// * `fd` - Socket descriptor
/// * `sockopt` - Socket option to be retrieved
pub fn sock_get_opt_flag<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    opt: __wasi_sockoption_t,
    ret_flag: WasmPtr<__wasi_bool_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::sock_get_opt_flag(ty={})", opt);
    let env = ctx.data();
    let memory = env.memory_view(&ctx);

    let option: super::state::WasiSocketOption = opt.into();
    let flag = wasi_try!(__sock_actor(&ctx, sock, 0, |socket| {
        socket.get_opt_flag(option)
    }));
    let flag = match flag {
        false => __WASI_BOOL_FALSE,
        true => __WASI_BOOL_TRUE,
    };

    wasi_try_mem!(ret_flag.write(&memory, flag));

    __WASI_ESUCCESS
}

/// ### `sock_set_opt_time()`
/// Sets one of the times the socket
///
/// ## Parameters
///
/// * `fd` - Socket descriptor
/// * `sockopt` - Socket option to be set
/// * `time` - Value to set the time to
pub fn sock_set_opt_time<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    opt: __wasi_sockoption_t,
    time: WasmPtr<__wasi_option_timestamp_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::sock_set_opt_time(ty={})", opt);

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let time = wasi_try_mem!(time.read(&memory));
    let time = match time.tag {
        __WASI_OPTION_NONE => None,
        __WASI_OPTION_SOME => Some(Duration::from_nanos(time.u)),
        _ => return __WASI_EINVAL,
    };

    let ty = match opt {
        __WASI_SOCK_OPTION_RECV_TIMEOUT => wasmer_vnet::TimeType::ReadTimeout,
        __WASI_SOCK_OPTION_SEND_TIMEOUT => wasmer_vnet::TimeType::WriteTimeout,
        __WASI_SOCK_OPTION_CONNECT_TIMEOUT => wasmer_vnet::TimeType::ConnectTimeout,
        __WASI_SOCK_OPTION_ACCEPT_TIMEOUT => wasmer_vnet::TimeType::AcceptTimeout,
        __WASI_SOCK_OPTION_LINGER => wasmer_vnet::TimeType::Linger,
        _ => return __WASI_EINVAL,
    };

    let option: super::state::WasiSocketOption = opt.into();
    wasi_try!(__sock_actor_mut(&ctx, sock, 0, |socket| {
        socket.set_opt_time(ty, time)
    }));
    __WASI_ESUCCESS
}

/// ### `sock_get_opt_time()`
/// Retrieve one of the times on the socket
///
/// ## Parameters
///
/// * `fd` - Socket descriptor
/// * `sockopt` - Socket option to be retrieved
pub fn sock_get_opt_time<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    opt: __wasi_sockoption_t,
    ret_time: WasmPtr<__wasi_option_timestamp_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::sock_get_opt_time(ty={})", opt);
    let env = ctx.data();
    let memory = env.memory_view(&ctx);

    let ty = match opt {
        __WASI_SOCK_OPTION_RECV_TIMEOUT => wasmer_vnet::TimeType::ReadTimeout,
        __WASI_SOCK_OPTION_SEND_TIMEOUT => wasmer_vnet::TimeType::WriteTimeout,
        __WASI_SOCK_OPTION_CONNECT_TIMEOUT => wasmer_vnet::TimeType::ConnectTimeout,
        __WASI_SOCK_OPTION_ACCEPT_TIMEOUT => wasmer_vnet::TimeType::AcceptTimeout,
        __WASI_SOCK_OPTION_LINGER => wasmer_vnet::TimeType::Linger,
        _ => return __WASI_EINVAL,
    };

    let time = wasi_try!(__sock_actor(&ctx, sock, 0, |socket| {
        socket.opt_time(ty)
    }));
    let time = match time {
        None => __wasi_option_timestamp_t {
            tag: __WASI_OPTION_NONE,
            u: 0,
        },
        Some(timeout) => __wasi_option_timestamp_t {
            tag: __WASI_OPTION_SOME,
            u: timeout.as_nanos() as __wasi_timestamp_t,
        },
    };

    wasi_try_mem!(ret_time.write(&memory, time));

    __WASI_ESUCCESS
}

/// ### `sock_set_opt_size()
/// Set size of particular option for this socket
/// Note: This is similar to `setsockopt` in POSIX for SO_RCVBUF
///
/// ## Parameters
///
/// * `fd` - Socket descriptor
/// * `opt` - Socket option to be set
/// * `size` - Buffer size
pub fn sock_set_opt_size(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    opt: __wasi_sockoption_t,
    size: __wasi_filesize_t,
) -> __wasi_errno_t {
    debug!("wasi::sock_set_opt_size(ty={})", opt);

    let ty = match opt {
        __WASI_SOCK_OPTION_RECV_TIMEOUT => wasmer_vnet::TimeType::ReadTimeout,
        __WASI_SOCK_OPTION_SEND_TIMEOUT => wasmer_vnet::TimeType::WriteTimeout,
        __WASI_SOCK_OPTION_CONNECT_TIMEOUT => wasmer_vnet::TimeType::ConnectTimeout,
        __WASI_SOCK_OPTION_ACCEPT_TIMEOUT => wasmer_vnet::TimeType::AcceptTimeout,
        __WASI_SOCK_OPTION_LINGER => wasmer_vnet::TimeType::Linger,
        _ => return __WASI_EINVAL,
    };

    let option: super::state::WasiSocketOption = opt.into();
    wasi_try!(__sock_actor_mut(&ctx, sock, 0, |socket| {
        match opt {
            __WASI_SOCK_OPTION_RECV_BUF_SIZE => socket.set_recv_buf_size(size as usize),
            __WASI_SOCK_OPTION_SEND_BUF_SIZE => socket.set_send_buf_size(size as usize),
            __WASI_SOCK_OPTION_TTL => socket.set_ttl(size as u32),
            __WASI_SOCK_OPTION_MULTICAST_TTL_V4 => socket.set_multicast_ttl_v4(size as u32),
            _ => Err(__WASI_EINVAL),
        }
    }));
    __WASI_ESUCCESS
}

/// ### `sock_get_opt_size()`
/// Retrieve the size of particular option for this socket
/// Note: This is similar to `getsockopt` in POSIX for SO_RCVBUF
///
/// ## Parameters
///
/// * `fd` - Socket descriptor
/// * `sockopt` - Socket option to be retrieved
pub fn sock_get_opt_size<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    opt: __wasi_sockoption_t,
    ret_size: WasmPtr<__wasi_filesize_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::sock_get_opt_size(ty={})", opt);
    let env = ctx.data();
    let memory = env.memory_view(&ctx);

    let size = wasi_try!(__sock_actor(&ctx, sock, 0, |socket| {
        match opt {
            __WASI_SOCK_OPTION_RECV_BUF_SIZE => {
                socket.recv_buf_size().map(|a| a as __wasi_filesize_t)
            }
            __WASI_SOCK_OPTION_SEND_BUF_SIZE => {
                socket.send_buf_size().map(|a| a as __wasi_filesize_t)
            }
            __WASI_SOCK_OPTION_TTL => socket.ttl().map(|a| a as __wasi_filesize_t),
            __WASI_SOCK_OPTION_MULTICAST_TTL_V4 => {
                socket.multicast_ttl_v4().map(|a| a as __wasi_filesize_t)
            }
            _ => Err(__WASI_EINVAL),
        }
    }));
    wasi_try_mem!(ret_size.write(&memory, size));

    __WASI_ESUCCESS
}

/// ### `sock_join_multicast_v4()`
/// Joins a particular multicast IPv4 group
///
/// ## Parameters
///
/// * `fd` - Socket descriptor
/// * `multiaddr` - Multicast group to joined
/// * `interface` - Interface that will join
pub fn sock_join_multicast_v4<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    multiaddr: WasmPtr<__wasi_addr_ip4_t, M>,
    iface: WasmPtr<__wasi_addr_ip4_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::sock_join_multicast_v4");

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let multiaddr = wasi_try!(super::state::read_ip_v4(&memory, multiaddr));
    let iface = wasi_try!(super::state::read_ip_v4(&memory, iface));
    wasi_try!(__sock_actor_mut(&ctx, sock, 0, |socket| {
        socket.join_multicast_v4(multiaddr, iface)
    }));
    __WASI_ESUCCESS
}

/// ### `sock_leave_multicast_v4()`
/// Leaves a particular multicast IPv4 group
///
/// ## Parameters
///
/// * `fd` - Socket descriptor
/// * `multiaddr` - Multicast group to leave
/// * `interface` - Interface that will left
pub fn sock_leave_multicast_v4<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    multiaddr: WasmPtr<__wasi_addr_ip4_t, M>,
    iface: WasmPtr<__wasi_addr_ip4_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::sock_leave_multicast_v4");

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let multiaddr = wasi_try!(super::state::read_ip_v4(&memory, multiaddr));
    let iface = wasi_try!(super::state::read_ip_v4(&memory, iface));
    wasi_try!(__sock_actor_mut(&ctx, sock, 0, |socket| {
        socket.leave_multicast_v4(multiaddr, iface)
    }));
    __WASI_ESUCCESS
}

/// ### `sock_join_multicast_v6()`
/// Joins a particular multicast IPv6 group
///
/// ## Parameters
///
/// * `fd` - Socket descriptor
/// * `multiaddr` - Multicast group to joined
/// * `interface` - Interface that will join
pub fn sock_join_multicast_v6<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    multiaddr: WasmPtr<__wasi_addr_ip6_t, M>,
    iface: u32,
) -> __wasi_errno_t {
    debug!("wasi::sock_join_multicast_v6");

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let multiaddr = wasi_try!(super::state::read_ip_v6(&memory, multiaddr));
    wasi_try!(__sock_actor_mut(&ctx, sock, 0, |socket| {
        socket.join_multicast_v6(multiaddr, iface)
    }));
    __WASI_ESUCCESS
}

/// ### `sock_leave_multicast_v6()`
/// Leaves a particular multicast IPv6 group
///
/// ## Parameters
///
/// * `fd` - Socket descriptor
/// * `multiaddr` - Multicast group to leave
/// * `interface` - Interface that will left
pub fn sock_leave_multicast_v6<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    multiaddr: WasmPtr<__wasi_addr_ip6_t, M>,
    iface: u32,
) -> __wasi_errno_t {
    debug!("wasi::sock_leave_multicast_v6");

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let multiaddr = wasi_try!(super::state::read_ip_v6(&memory, multiaddr));
    wasi_try!(__sock_actor_mut(&ctx, sock, 0, |socket| {
        socket.leave_multicast_v6(multiaddr, iface)
    }));
    __WASI_ESUCCESS
}

/// ### `sock_bind()`
/// Bind a socket
/// Note: This is similar to `bind` in POSIX using PF_INET
///
/// ## Parameters
///
/// * `fd` - File descriptor of the socket to be bind
/// * `addr` - Address to bind the socket to
pub fn sock_bind<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    addr: WasmPtr<__wasi_addr_port_t, M>,
) -> __wasi_errno_t {
    debug!("wasi::sock_bind");

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let addr = wasi_try!(super::state::read_ip_port(&memory, addr));
    let addr = SocketAddr::new(addr.0, addr.1);
    wasi_try!(__sock_upgrade(
        &ctx,
        sock,
        __WASI_RIGHT_SOCK_BIND,
        |socket| { socket.bind(env.net(), addr) }
    ));
    __WASI_ESUCCESS
}

/// ### `sock_listen()`
/// Listen for connections on a socket
///
/// Polling the socket handle will wait until a connection
/// attempt is made
///
/// Note: This is similar to `listen`
///
/// ## Parameters
///
/// * `fd` - File descriptor of the socket to be bind
/// * `backlog` - Maximum size of the queue for pending connections
pub fn sock_listen<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    backlog: M::Offset,
) -> __wasi_errno_t {
    debug!("wasi::sock_listen");

    let env = ctx.data();
    let backlog: usize = wasi_try!(backlog.try_into().map_err(|_| __WASI_EINVAL));
    wasi_try!(__sock_upgrade(
        &ctx,
        sock,
        __WASI_RIGHT_SOCK_BIND,
        |socket| { socket.listen(env.net(), backlog) }
    ));
    __WASI_ESUCCESS
}

/// ### `sock_accept()`
/// Accept a new incoming connection.
/// Note: This is similar to `accept` in POSIX.
///
/// ## Parameters
///
/// * `fd` - The listening socket.
/// * `flags` - The desired values of the file descriptor flags.
///
/// ## Return
///
/// New socket connection
pub fn sock_accept<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    fd_flags: __wasi_fdflags_t,
    ro_fd: WasmPtr<__wasi_fd_t, M>,
    ro_addr: WasmPtr<__wasi_addr_port_t, M>,
) -> Result<__wasi_errno_t, WasiError> {
    debug!("wasi::sock_accept");

    let mut env = ctx.data();
    let (child, addr) = {
        let mut ret;
        let (_, state) = env.get_memory_and_wasi_state(&ctx, 0);
        loop {
            wasi_try_ok!(
                match __sock_actor(&ctx, sock, __WASI_RIGHT_SOCK_ACCEPT, |socket| socket
                    .accept_timeout(fd_flags, Duration::from_millis(5)))
                {
                    Ok(a) => {
                        ret = a;
                        break;
                    }
                    Err(__WASI_ETIMEDOUT) => {
                        env.yield_now()?;
                        continue;
                    }
                    Err(__WASI_EAGAIN) => {
                        env.clone().sleep(&mut ctx, Duration::from_millis(5))?;
                        env = ctx.data();
                        continue;
                    }
                    Err(err) => Err(err),
                }
            );
        }
        ret
    };

    let (memory, state, mut inodes) = env.get_memory_and_wasi_state_and_inodes_mut(&ctx, 0);

    let kind = Kind::Socket {
        socket: InodeSocket::new(InodeSocketKind::TcpStream(child)),
    };
    let inode = state.fs.create_inode_with_default_stat(
        inodes.deref_mut(),
        kind,
        false,
        "socket".into(),
    );

    let rights = super::state::all_socket_rights();
    let fd = wasi_try_ok!(state.fs.create_fd(rights, rights, 0, 0, inode));

    wasi_try_mem_ok!(ro_fd.write(&memory, fd));
    wasi_try_ok!(super::state::write_ip_port(
        &memory,
        ro_addr,
        addr.ip(),
        addr.port()
    ));

    Ok(__WASI_ESUCCESS)
}

/// ### `sock_connect()`
/// Initiate a connection on a socket to the specified address
///
/// Polling the socket handle will wait for data to arrive or for
/// the socket status to change which can be queried via 'sock_status'
///
/// Note: This is similar to `connect` in POSIX
///
/// ## Parameters
///
/// * `fd` - Socket descriptor
/// * `addr` - Address of the socket to connect to
pub fn sock_connect<M: MemorySize>(
    ctx: FunctionEnvMut<WasiEnv>,
    sock: __wasi_fd_t,
    addr: WasmPtr<__wasi_addr_port_t, M>,
) -> __wasi_errno_t {
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let addr = wasi_try!(super::state::read_ip_port(&memory, addr));
    let addr = SocketAddr::new(addr.0, addr.1);
    wasi_try!(__sock_upgrade(
        &ctx,
        sock,
        __WASI_RIGHT_SOCK_CONNECT,
        |socket| { socket.connect(env.net(), addr) }
    ));
    __WASI_ESUCCESS
}

/// ### `sock_recv()`
/// Receive a message from a socket.
/// Note: This is similar to `recv` in POSIX, though it also supports reading
/// the data into multiple buffers in the manner of `readv`.
///
/// ## Parameters
///
/// * `ri_data` - List of scatter/gather vectors to which to store data.
/// * `ri_flags` - Message flags.
///
/// ## Return
///
/// Number of bytes stored in ri_data and message flags.
pub fn sock_recv<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    ri_data: WasmPtr<__wasi_iovec_t<M>, M>,
    ri_data_len: M::Offset,
    _ri_flags: __wasi_riflags_t,
    ro_data_len: WasmPtr<M::Offset, M>,
    ro_flags: WasmPtr<__wasi_roflags_t, M>,
) -> Result<__wasi_errno_t, WasiError> {
    debug!("wasi::sock_recv");

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let iovs_arr = wasi_try_mem_ok!(ri_data.slice(&memory, ri_data_len));

    let bytes_read = wasi_try_ok!(__sock_actor_mut(
        &ctx,
        sock,
        __WASI_RIGHT_SOCK_RECV,
        |socket| { socket.recv(& memory, iovs_arr) }
    ));
    let bytes_read: M::Offset = wasi_try_ok!(bytes_read.try_into().map_err(|_| __WASI_EOVERFLOW));

    wasi_try_mem_ok!(ro_flags.write(&memory, 0));
    wasi_try_mem_ok!(ro_data_len.write(&memory, bytes_read));

    Ok(__WASI_ESUCCESS)
}

/// ### `sock_recv_from()`
/// Receive a message and its peer address from a socket.
/// Note: This is similar to `recvfrom` in POSIX, though it also supports reading
/// the data into multiple buffers in the manner of `readv`.
///
/// ## Parameters
///
/// * `ri_data` - List of scatter/gather vectors to which to store data.
/// * `ri_flags` - Message flags.
///
/// ## Return
///
/// Number of bytes stored in ri_data and message flags.
pub fn sock_recv_from<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    ri_data: WasmPtr<__wasi_iovec_t<M>, M>,
    ri_data_len: M::Offset,
    _ri_flags: __wasi_riflags_t,
    ro_data_len: WasmPtr<M::Offset, M>,
    ro_flags: WasmPtr<__wasi_roflags_t, M>,
    ro_addr: WasmPtr<__wasi_addr_port_t, M>,
) -> Result<__wasi_errno_t, WasiError> {
    debug!("wasi::sock_recv_from");

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let iovs_arr = wasi_try_mem_ok!(ri_data.slice(&memory, ri_data_len));

    let bytes_read = wasi_try_ok!(__sock_actor_mut(
        &ctx,
        sock,
        __WASI_RIGHT_SOCK_RECV_FROM,
        |socket| { socket.recv_from(&memory, iovs_arr, ro_addr) }
    ));
    let bytes_read: M::Offset = wasi_try_ok!(bytes_read.try_into().map_err(|_| __WASI_EOVERFLOW));

    wasi_try_mem_ok!(ro_flags.write(&memory, 0));
    wasi_try_mem_ok!(ro_data_len.write(&memory, bytes_read));

    Ok(__WASI_ESUCCESS)
}

/// ### `sock_send()`
/// Send a message on a socket.
/// Note: This is similar to `send` in POSIX, though it also supports writing
/// the data from multiple buffers in the manner of `writev`.
///
/// ## Parameters
///
/// * `si_data` - List of scatter/gather vectors to which to retrieve data
/// * `si_flags` - Message flags.
///
/// ## Return
///
/// Number of bytes transmitted.
pub fn sock_send<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    si_data: WasmPtr<__wasi_ciovec_t<M>, M>,
    si_data_len: M::Offset,
    _si_flags: __wasi_siflags_t,
    ret_data_len: WasmPtr<M::Offset, M>,
) -> Result<__wasi_errno_t, WasiError> {
    debug!("wasi::sock_send");
    let env = ctx.data();

    let memory = env.memory_view(&ctx);
    let iovs_arr = wasi_try_mem_ok!(si_data.slice(&memory, si_data_len));

    let bytes_written = wasi_try_ok!(__sock_actor_mut(
        &ctx,
        sock,
        __WASI_RIGHT_SOCK_SEND,
        |socket| { socket.send(&memory, iovs_arr) }
    ));

    let bytes_written: M::Offset =
        wasi_try_ok!(bytes_written.try_into().map_err(|_| __WASI_EOVERFLOW));
    wasi_try_mem_ok!(ret_data_len.write(&memory, bytes_written));

    Ok(__WASI_ESUCCESS)
}

/// ### `sock_send_to()`
/// Send a message on a socket to a specific address.
/// Note: This is similar to `sendto` in POSIX, though it also supports writing
/// the data from multiple buffers in the manner of `writev`.
///
/// ## Parameters
///
/// * `si_data` - List of scatter/gather vectors to which to retrieve data
/// * `si_flags` - Message flags.
/// * `addr` - Address of the socket to send message to
///
/// ## Return
///
/// Number of bytes transmitted.
pub fn sock_send_to<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    si_data: WasmPtr<__wasi_ciovec_t<M>, M>,
    si_data_len: M::Offset,
    _si_flags: __wasi_siflags_t,
    addr: WasmPtr<__wasi_addr_port_t, M>,
    ret_data_len: WasmPtr<M::Offset, M>,
) -> Result<__wasi_errno_t, WasiError> {
    debug!("wasi::sock_send_to");
    let env = ctx.data();

    let memory = env.memory_view(&ctx);
    let iovs_arr = wasi_try_mem_ok!(si_data.slice(&memory, si_data_len));

    let bytes_written = wasi_try_ok!(__sock_actor_mut(
        &ctx,
        sock,
        __WASI_RIGHT_SOCK_SEND_TO,
        |socket| { socket.send_to::<M>(&memory, iovs_arr, addr) }
    ));

    let bytes_written: M::Offset =
        wasi_try_ok!(bytes_written.try_into().map_err(|_| __WASI_EOVERFLOW));
    wasi_try_mem_ok!(ret_data_len.write(&memory, bytes_written as M::Offset));

    Ok(__WASI_ESUCCESS)
}

/// ### `sock_send_file()`
/// Sends the entire contents of a file down a socket
///
/// ## Parameters
///
/// * `in_fd` - Open file that has the data to be transmitted
/// * `offset` - Offset into the file to start reading at
/// * `count` - Number of bytes to be sent
///
/// ## Return
///
/// Number of bytes transmitted.
pub fn sock_send_file<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: __wasi_fd_t,
    in_fd: __wasi_fd_t,
    offset: __wasi_filesize_t,
    mut count: __wasi_filesize_t,
    ret_sent: WasmPtr<__wasi_filesize_t, M>,
) -> Result<__wasi_errno_t, WasiError> {
    debug!("wasi::send_file");
    let env = ctx.data();
    let (memory, mut state, inodes) = env.get_memory_and_wasi_state_and_inodes(&ctx, 0);

    // Set the offset of the file
    {
        let mut fd_map = state.fs.fd_map.write().unwrap();
        let fd_entry = wasi_try_ok!(fd_map.get_mut(&in_fd).ok_or(__WASI_EBADF));
        fd_entry.offset = offset as u64;
    }

    // Enter a loop that will process all the data
    let mut total_written: __wasi_filesize_t = 0;
    while (count > 0) {
        let mut buf = [0; 4096];
        let sub_count = count.min(4096);
        count -= sub_count;

        let fd_entry = wasi_try_ok!(state.fs.get_fd(in_fd));
        let bytes_read = match in_fd {
            __WASI_STDIN_FILENO => {
                let mut guard = wasi_try_ok!(
                    inodes
                        .stdin_mut(&state.fs.fd_map)
                        .map_err(fs_error_into_wasi_err),
                    env
                );
                if let Some(ref mut stdin) = guard.deref_mut() {
                    wasi_try_ok!(stdin.read(&mut buf).map_err(map_io_err))
                } else {
                    return Ok(__WASI_EBADF);
                }
            }
            __WASI_STDOUT_FILENO | __WASI_STDERR_FILENO => return Ok(__WASI_EINVAL),
            _ => {
                if !has_rights(fd_entry.rights, __WASI_RIGHT_FD_READ) {
                    // TODO: figure out the error to return when lacking rights
                    return Ok(__WASI_EACCES);
                }

                let offset = fd_entry.offset as usize;
                let inode_idx = fd_entry.inode;
                let inode = &inodes.arena[inode_idx];

                let bytes_read = {
                    let mut guard = inode.write();
                    match guard.deref_mut() {
                        Kind::File { handle, .. } => {
                            if let Some(handle) = handle {
                                wasi_try_ok!(
                                    handle
                                        .seek(std::io::SeekFrom::Start(offset as u64))
                                        .map_err(map_io_err),
                                    env
                                );
                                wasi_try_ok!(handle.read(&mut buf).map_err(map_io_err))
                            } else {
                                return Ok(__WASI_EINVAL);
                            }
                        }
                        Kind::Socket { socket } => {
                            wasi_try_ok!(socket.read(&mut buf).map_err(map_io_err))
                        }
                        Kind::Pipe { pipe } => {
                            wasi_try_ok!(pipe.read(&mut buf).map_err(map_io_err))
                        }
                        Kind::Dir { .. } | Kind::Root { .. } => {
                            return Ok(__WASI_EISDIR);
                        }
                        Kind::EventNotifications { .. } => {
                            return Ok(__WASI_EINVAL);
                        }
                        Kind::Symlink { .. } => unimplemented!("Symlinks in wasi::fd_read"),
                        Kind::Buffer { buffer } => {
                            let mut buf_read = &buffer[offset..];
                            wasi_try_ok!(buf_read.read(&mut buf).map_err(map_io_err))
                        }
                    }
                };

                // reborrow
                let mut fd_map = state.fs.fd_map.write().unwrap();
                let fd_entry = wasi_try_ok!(fd_map.get_mut(&in_fd).ok_or(__WASI_EBADF));
                fd_entry.offset += bytes_read as u64;

                bytes_read
            }
        };

        // Write it down to the socket
        let bytes_written = wasi_try_ok!(__sock_actor_mut(
            &ctx,
            sock,
            __WASI_RIGHT_SOCK_SEND,
            |socket| {
                let buf = (&buf[..]).to_vec();
                socket.send_bytes::<M>(Bytes::from(buf))
            }
        ));
        total_written += bytes_written as u64;
    }

    wasi_try_mem_ok!(ret_sent.write(&memory, total_written as __wasi_filesize_t));

    Ok(__WASI_ESUCCESS)
}

/// ### `resolve()`
/// Resolves a hostname and a port to one or more IP addresses.
///
/// Note: This is similar to `getaddrinfo` in POSIX
///
/// When successful, the contents of the output buffer consist of a sequence of
/// IPv4 and/or IPv6 addresses. Each address entry consists of a addr_t object.
/// This function fills the output buffer as much as possible.
///
/// ## Parameters
///
/// * `host` - Host to resolve
/// * `port` - Port hint (zero if no hint is supplied)
/// * `addrs` - The buffer where addresses will be stored
///
/// ## Return
///
/// The number of IP addresses returned during the DNS resolution.
pub fn resolve<M: MemorySize>(
    ctx: FunctionEnvMut<'_, WasiEnv>,
    host: WasmPtr<u8, M>,
    host_len: M::Offset,
    port: u16,
    addrs: WasmPtr<__wasi_addr_t, M>,
    naddrs: M::Offset,
    ret_naddrs: WasmPtr<M::Offset, M>,
) -> __wasi_errno_t {
    debug!("wasi::resolve");

    let naddrs: usize = wasi_try!(naddrs.try_into().map_err(|_| __WASI_EINVAL));
    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let host_str = unsafe { get_input_str!(&memory, host, host_len) };
    let addrs = wasi_try_mem!(addrs.slice(&memory, wasi_try!(to_offset::<M>(naddrs))));

    let port = if port > 0 { Some(port) } else { None };

    let found_ips = wasi_try!(env
        .net()
        .resolve(host_str.as_str(), port, None)
        .map_err(net_error_into_wasi_err));

    let mut idx = 0;
    for found_ip in found_ips.iter().take(naddrs) {
        super::state::write_ip(&memory, addrs.index(idx).as_ptr::<M>(), *found_ip);
        idx += 1;
    }

    let idx: M::Offset = wasi_try!(idx.try_into().map_err(|_| __WASI_EOVERFLOW));
    wasi_try_mem!(ret_naddrs.write(&memory, idx));

    __WASI_ESUCCESS
}
