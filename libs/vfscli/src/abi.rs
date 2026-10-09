//! Costanti ABI per il client FS (S2.2): re-export da `syscall-numbers`.
//!
//! Nel repo questa e' la single source transitiva; nella copia vendored in
//! `std` (`sys/fs/vfslib/abi.rs`, generata da `apply.py`) stanno gli stessi
//! nomi come letterali. I file `session`/`ring`/`sync`/`cwd` usano SOLO
//! `super::abi::*`: identici nei due posti, mai derive silenziose.

pub use syscall_numbers::{
    CHANNEL_PARENT, CLI_REQ_VA, CLI_RESP_VA, CWD_MAX, ERR, ERR_BUSY, ERR_CLOSED, ERR_EMPTY,
    ERR_EXISTS, ERR_INVALID, ERR_ISDIR, ERR_NOHANDSHAKE, ERR_NOSPC, ERR_NOTDIR, ERR_NOTFOUND,
    ERR_READONLY, FS_BUF_REG, FS_NOTIFY, FS_REGISTER, OPEN_APPEND, OPEN_CREATE, OPEN_TRUNC,
    R_CHMOD, R_CLOSE,
    R_DELETE, R_DISK_INFO, R_DISK_LIST, R_DUP_CANCEL, R_DUP_CLAIM, R_DUP_GRANT, R_GET_HASH,
    R_LSEEK, R_MKDIR, R_MOUNT, R_OPEN, R_PIPE_CREATE, R_READ, R_READDIR, R_READLINK, R_REGISTER,
    R_RENAME, R_RIGHTS_DROP, R_RIGHTS_GET, R_STAT, R_STATVFS, R_SYMLINK, R_SYNC, R_UMOUNT, R_WRITE,
    SEEK_CURRENT, SEEK_END, SEEK_START, STAT_DEVICE, STAT_DIR, STAT_FILE, STAT_READONLY,
    STAT_SYMLINK, SVC_READY, USER_FS_BUFFER, USER_RESP_RING,
};
