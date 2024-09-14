use crate::utils;
use crate::utils::{Event, EventType, Fd, TId, VmId};
use std::{
    collections::{HashMap, HashSet},
    os::raw::{c_int, c_void},
    sync::Mutex,
};

lazy_static! {
    pub(crate) static ref PROCESS_INFOS: Mutex<ProcessInfos> = Mutex::new(ProcessInfos::default());
}
static mut PROCESS_INFOS_PTR: *mut c_void = std::ptr::null_mut();
static mut PID: Option<VmId> = None;

pub(crate) struct ProcessInfo {
    pid: VmId,
    child_id: HashSet<VmId>,
    inherited_fds: Option<Vec<Fd>>,
}
impl ProcessInfo {
    fn new_parent() -> Self {
        Self {
            pid: 0,
            child_id: Default::default(),
            inherited_fds: None,
        }
    }
}

pub(crate) struct ProcessInfos {
    fds_count: u64,
    pid_count: u64,
    processes: HashMap<VmId, ProcessInfo>,
    fds: HashMap<Fd, VmId>,
    event: Event,
}
impl Default for ProcessInfos {
    fn default() -> Self {
        let mut processes = HashMap::new();
        processes.insert(0, ProcessInfo::new_parent());
        Self {
            fds_count: 2,
            pid_count: 1,
            processes: processes,
            fds: HashMap::new(),
            event: Event::default(),
        }
    }
}
impl ProcessInfos {
    fn get() -> &'static Mutex<ProcessInfos> {
        if unsafe { PROCESS_INFOS_PTR.is_null() } {
            &PROCESS_INFOS
        } else {
            unsafe {
                let ptr = PROCESS_INFOS_PTR as *mut Mutex<ProcessInfos>;
                let ptr2: &mut Mutex<ProcessInfos> = &mut *ptr;

                ptr2
            }
        }
    }
    fn get_pid(&self) -> VmId {
        unsafe {
            if PID.is_none() {
                0
            } else {
                PID.as_ref().unwrap().clone()
            }
        }
    }

    fn get_process_info(&self) -> &ProcessInfo {
        self.processes.get(&self.get_pid()).unwrap()
    }

    fn get_new_pid(&mut self) -> VmId {
        let id = self.pid_count;
        self.pid_count += 1;
        id
    }

    fn new_pipe(&mut self) -> (Fd, Fd) {
        let pid = self.get_pid();
        let fds = Fd::create(self.fds_count);

        self.fds.insert(fds.0, pid);
        self.fds.insert(fds.1, pid);

        self.fds_count = fds.2;

        (fds.0, fds.1)
    }

    fn close_pipe(&mut self, fd: Fd) -> bool {
        let h = self.fds.get(&fd);
        if h.is_none() {
            return false;
        }
        if h.unwrap() != &self.get_pid() {
            return false;
        }
        self.fds.remove(&fd).is_some()
    }
}

#[no_mangle]
pub extern "C" fn update_spawn_info(ptr: *mut std::ffi::c_void, pid: u64) {
    unsafe {
        PROCESS_INFOS_PTR = ptr;
        PID = Some(pid)
    }
}

#[repr(C)]
#[derive(Clone)]
pub struct SpawnArgs {
    /// argc contains the number of arguments passed to the program.
    pub argc: u64,
    /// argv is a one-dimensional array of strings.
    pub argv: *const *const i8,
    /// a pointer used to save the process_id of the child process.
    pub process_id: *mut u64,
    /// an array representing the file descriptors passed to the child process. It must end with zero.
    pub inherited_fds: *const u64,
}

#[no_mangle]
pub extern "C" fn ckb_spawn(
    index: usize,
    source: usize,
    place: usize,
    bounds: usize,
    spgs: *mut SpawnArgs,
) -> c_int {
    let hash = utils::get_hash(index, source, place, bounds);
    if hash.is_err() {
        return hash.unwrap_err();
    }
    let native_bin_path = utils::get_bin_path_by_hash(hash.unwrap());

    let spgs = unsafe { &mut *spgs };
    let inherited_fds: Vec<u64> = unsafe {
        let mut fds = Vec::new();
        let mut fds_ptr = spgs.inherited_fds;
        while *fds_ptr != 0 {
            fds.push(*fds_ptr);
            fds_ptr = fds_ptr.add(1);
        }
        fds
    };
    let (_pid, spawn_pid, event) = {
        let mut infos = ProcessInfos::get().lock().unwrap();
        let pid = infos.get_pid();
        let spawn_pid = infos.get_new_pid();

        inherited_fds.iter().all(|fd| {
            *infos.fds.get_mut(&(*fd).into()).unwrap() = spawn_pid;
            true
        });

        let event = infos.event.clone();
        (pid, spawn_pid, event)
    };

    let args = {
        let mut args = Vec::with_capacity(spgs.argc as usize);
        for i in 0..spgs.argc {
            let c_str = unsafe { std::ffi::CStr::from_ptr(*spgs.argv.add(i as usize)) };
            let str_slice = c_str
                .to_str()
                .expect("Failed to convert C string to Rust string");
            args.push(str_slice.to_owned());
        }
        args
    };

    std::thread::spawn(move || {
        let lib = utils::CkbNativeSimulatorLib::new(&native_bin_path);
        lib.update_spawn_info(spawn_pid);
        let mut event = {
            let mut infos = ProcessInfos::get().lock().unwrap();
            infos.processes.insert(
                spawn_pid,
                ProcessInfo {
                    pid: spawn_pid,
                    child_id: Default::default(),
                    inherited_fds: Some(inherited_fds.iter().map(|f| (*f).into()).collect()),
                },
            );
            infos.event.clone()
        };

        let args = args.clone();
        let argc = args.len() as u64;
        let mut argv: Vec<*const i8> = Vec::with_capacity(argc as usize + 1);
        for s in args {
            let c_string = std::ffi::CString::new(s).expect("CString::new failed");
            argv.push(c_string.into_raw());
        }
        argv.push(std::ptr::null_mut());
        let rc = lib.ckb_std_main(argc as i32, argv.as_ptr());
        event.notify(EventType::ProcessExit((spawn_pid, rc)));
    });
    unsafe { *(&mut *spgs).process_id = spawn_pid.clone() };

    event.wait(move |e| match e {
        EventType::ProcessExit((pid, _rc)) => {
            if pid == spawn_pid {
                true
            } else {
                false
            }
        }
        _ => false,
    });
    0
}

#[no_mangle]
pub extern "C" fn ckb_wait(_pid: u64, _code: *mut i8) -> c_int {
    panic!("unsupport");
}

#[no_mangle]
pub extern "C" fn ckb_process_id() -> u64 {
    ProcessInfos::get().lock().unwrap().get_pid()
}

#[no_mangle]
pub extern "C" fn ckb_pipe(fds: *mut u64) -> c_int {
    let fds = unsafe { std::slice::from_raw_parts_mut(fds, 2) };
    let out = ProcessInfos::get().lock().unwrap().new_pipe();

    fds[0] = out.0 .0;
    fds[1] = out.1 .0;
    0
}

#[no_mangle]
pub extern "C" fn ckb_read(_fd: u64, _buf: *mut c_void, _length: *mut usize) -> c_int {
    panic!("unsupport");
}

#[no_mangle]
pub extern "C" fn ckb_write(_fd: u64, _buf: *const c_void, _length: *mut usize) -> c_int {
    panic!("unsupport");
}

#[no_mangle]
pub extern "C" fn ckb_inherited_fds(fds: *mut u64, length: *mut usize) -> c_int {
    let mut fds_ptr = fds;

    let fds: Vec<Fd> = ProcessInfos::get()
        .lock()
        .unwrap()
        .get_process_info()
        .inherited_fds
        .clone()
        .expect("get inherited fds");

    let len = fds.len().min(unsafe { *length });

    for i in 0..len {
        unsafe {
            *fds_ptr = fds[i].into();
            fds_ptr = fds_ptr.add(1);
        }
    }

    0
}

#[no_mangle]
pub extern "C" fn ckb_close(fd: u64) -> c_int {
    let r = ProcessInfos::get().lock().unwrap().close_pipe(fd.into());
    if r {
        0
    } else {
        6 // CKB_INVALID_FD
    }
}

#[no_mangle]
pub extern "C" fn ckb_load_block_extension(
    _addr: *mut c_void,
    _len: *mut u64,
    _offset: usize,
    _index: usize,
    _source: usize,
) -> c_int {
    panic!("unsupport");
}
