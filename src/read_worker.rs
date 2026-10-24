//! A persistent, read-only subprocess contains potentially blocking filesystem calls.
use std::{fs::File, io::{self, Read, Write}, os::unix::{net::UnixStream, io::{FromRawFd, AsRawFd}, ffi::OsStrExt}, path::Path, process::{Child, Command, Stdio}, sync::atomic::{AtomicBool, Ordering}, time::{Duration, Instant}};
const PAGE: usize = 8192;
pub struct Reader { socket: UnixStream, child: Child }
impl Reader {
    pub fn start() -> io::Result<Self> {
        let (socket, child_socket)=UnixStream::pair()?;
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;
        socket.set_write_timeout(Some(Duration::from_millis(100)))?;
        let fd: std::os::fd::OwnedFd=child_socket.into();
        let mut command=Command::new(std::env::current_exe()?);
        #[cfg(not(test))] command.arg("--directory-worker");
        #[cfg(test)] command.args(["--exact","read_worker::tests::fixture_worker","--ignored","--nocapture"]).env("KARUO_DIRECTORY_TEST_WORKER","1");
        let child=command.stdin(Stdio::from(fd)).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        Ok(Self{socket,child})
    }
    pub fn open(&mut self,path:&Path,device:u64)->io::Result<()> {
        let bytes=path.as_os_str().as_bytes();
        let n=u32::try_from(bytes.len()).map_err(|_|io::Error::other("目录路径太长"))?;
        self.socket.write_all(&n.to_le_bytes())?;
        self.socket.write_all(&device.to_le_bytes())?;
        self.socket.write_all(bytes)
    }
    pub fn page(&mut self,buf:&mut [u8;PAGE],cancel:&AtomicBool)->io::Result<i32> {
        // Timeout only applies to one response, never total scan time or file count.
        let deadline=Instant::now()+Duration::from_secs(3);
        let mut header=[0;4];
        self.read_until(&mut header,cancel,deadline)?;
        let count=i32::from_le_bytes(header);
        if count>0 {self.read_until(buf,cancel,deadline)?;}
        Ok(count)
    }
    fn read_until(&mut self,mut out:&mut [u8],cancel:&AtomicBool,deadline:Instant)->io::Result<()> {
        while !out.is_empty() {
            if cancel.load(Ordering::Relaxed) {return Err(io::Error::new(io::ErrorKind::Interrupted,"已停止读取"));}
            if Instant::now()>=deadline {return Err(io::Error::new(io::ErrorKind::TimedOut,"此目录读取超过 3 秒，已跳过并继续其他目录"));}
            match self.socket.read(out) {
                Ok(0)=>return Err(io::Error::new(io::ErrorKind::UnexpectedEof,"目录读取进程已退出")),
                Ok(n)=>{out=&mut out[n..];},
                Err(e) if matches!(e.kind(),io::ErrorKind::WouldBlock|io::ErrorKind::TimedOut|io::ErrorKind::Interrupted)=>{},
                Err(e)=>return Err(e),
            }
        } Ok(())
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        let _=self.child.kill();
        // Reaping happens off the scanner/UI thread, including pathological kernel I/O.
        if self.child.try_wait().ok().flatten().is_none() {
            let pid=self.child.id() as libc::pid_t;
            std::thread::spawn(move||unsafe {libc::waitpid(pid,std::ptr::null_mut(),0);});
        }
    }
}
#[cfg(target_os="macos")]
pub fn run()->io::Result<()> {
    use std::{ffi::{CString,OsString},os::unix::{ffi::OsStringExt,fs::MetadataExt}};
    const ATTR_CMN_NAME: u32 = 0x0000_0001;
    const ATTR_CMN_DEVID: u32 = 0x0000_0002;
    const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
    const ATTR_CMN_FILEID: u32 = 0x0200_0000;
    const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
    const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
    const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }
    unsafe extern "C" {
        fn getattrlistbulk(
            fd: libc::c_int,
            list: *const AttrList,
            buf: *mut libc::c_void,
            size: usize,
            options: u64,
        ) -> libc::c_int;
    }
    let attr_list = AttrList {
        bitmapcount: 5,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_NAME
            | ATTR_CMN_DEVID
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE,
        forkattr: 0,
    };

    let mut socket=unsafe{UnixStream::from_raw_fd(0)};
    loop {
        let mut n=[0;4];socket.read_exact(&mut n)?;
        let n=u32::from_le_bytes(n) as usize;
        if n==0 || n>1024*1024 {return Err(io::Error::other("invalid path frame"));}
        let mut device=[0;8];socket.read_exact(&mut device)?;
        let mut bytes=vec![0;n];socket.read_exact(&mut bytes)?;
        let path=OsString::from_vec(bytes);
        let name=CString::new(path.as_bytes()).map_err(io::Error::other)?;
        let fd=unsafe{libc::open(name.as_ptr(),libc::O_RDONLY|libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC)};
        if fd<0 {socket.write_all(&(-io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)).to_le_bytes())?;continue;}
        let file=unsafe{File::from_raw_fd(fd)};
        if file.metadata()?.dev()!=u64::from_le_bytes(device) {socket.write_all(&(-libc::EXDEV).to_le_bytes())?;continue;}
        loop {
            let mut buf=[0u8;PAGE];
            let count=unsafe{getattrlistbulk(file.as_raw_fd(),&attr_list,buf.as_mut_ptr().cast(),buf.len(),0)};
            let result=if count<0 {-io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)}else{count};
            socket.write_all(&result.to_le_bytes())?;
            if count<=0 {break;}
            socket.write_all(&buf)?;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore]
    fn fixture_worker() {
        if std::env::var_os("KARUO_DIRECTORY_TEST_WORKER").is_some() {let _=run();}
    }
    #[test]
    fn stalled_reader_can_be_cancelled_without_waiting_for_child() {
        let (socket,other)=UnixStream::pair().unwrap();socket.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let child=Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let mut reader=Reader{socket,child};let flag=AtomicBool::new(true);let started=Instant::now();
        assert_eq!(reader.page(&mut [0;PAGE],&flag).unwrap_err().kind(),io::ErrorKind::Interrupted);
        drop(reader);drop(other);assert!(started.elapsed()<Duration::from_secs(1));
    }
    #[test]
    fn stalled_response_times_out_and_next_reader_still_works() {
        let (socket,other)=UnixStream::pair().unwrap();socket.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let child=Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let mut reader=Reader{socket,child};let started=Instant::now();
        assert_eq!(reader.page(&mut [0;PAGE],&AtomicBool::new(false)).unwrap_err().kind(),io::ErrorKind::TimedOut);
        assert!(started.elapsed()<Duration::from_secs(4));drop(reader);drop(other);
        let mut next=Reader::start().unwrap();use std::os::unix::fs::MetadataExt;
        let root=std::env::temp_dir();next.open(&root,std::fs::metadata(&root).unwrap().dev()).unwrap();
        assert!(next.page(&mut [0;PAGE],&AtomicBool::new(false)).unwrap()>=0);
    }
    #[test]
    fn partial_frame_is_not_treated_as_complete() {
        let (socket,mut other)=UnixStream::pair().unwrap();socket.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let child=Command::new("/bin/sleep").arg("30").spawn().unwrap();
        other.write_all(&1i32.to_le_bytes()).unwrap();other.write_all(&[0;9]).unwrap();drop(other);
        let mut reader=Reader{socket,child};assert_eq!(reader.page(&mut [0;PAGE],&AtomicBool::new(false)).unwrap_err().kind(),io::ErrorKind::UnexpectedEof);
    }
}
