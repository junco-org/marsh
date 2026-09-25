use std::io;
use std::path::PathBuf;

#[path = "buffer_file_io/unix.rs"]
mod platform;

pub(crate) fn run_internal_fifo_reader_helper<I>(arguments: I) -> Option<i32>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    platform::run_internal_fifo_reader_helper(arguments)
}

pub(crate) async fn read(path: PathBuf) -> io::Result<Vec<u8>> {
    platform::read(path).await
}

pub(crate) async fn write(path: PathBuf, content: Vec<u8>, append: bool) -> io::Result<()> {
    platform::write(path, content, append).await
}
