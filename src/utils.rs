use crate::{fetch_cell, fetch_witness};
use ckb_types::{packed::Byte32, prelude::*};
use std::ffi::{c_int, c_void};

pub fn get_hash(index: usize, source: usize, place: usize, bounds: usize) -> Result<Byte32, c_int> {
    let bin = match place {
        0 => {
            let (_, cell_data) = match fetch_cell(index as u64, source as u64) {
                Ok(cell) => cell,
                Err(code) => return Err(code),
            };
            cell_data
        }
        1 => {
            let buf = fetch_witness(index as u64, source as u64);
            buf.unwrap().as_bytes()
        }
        _ => {
            println!("Invalid place value: {:#x}", place);
            return Err(1); //INDEX_OUT_OF_BOUND
        }
    };

    let offset = bounds >> 32;
    let length = bounds as u32 as u64;

    let offset = std::cmp::min(offset as usize, bin.len());
    let full_length = bin.len() - offset;
    let real_length = if length > 0 {
        std::cmp::min(full_length, length as usize)
    } else {
        full_length
    };
    let bin = bin.slice(offset..offset + real_length);

    Ok(Byte32::new(ckb_hash::blake2b_256(bin)))
}

pub fn get_bin_path_by_hash(hash: Byte32) -> String {
    let hash = faster_hex::hex_string(&hash.raw_data().to_vec());

    let native_bin_path = crate::SETUP.native_binaries.get(&hash);
    if native_bin_path.is_none() {
        panic!("Native-simulator not found, code hash: {}", &hash);
    }
    native_bin_path.unwrap().clone()
}

pub type VmId = u64;
pub type TId = u64;

pub struct CkbNativeSimulatorLib {
    lib: libloading::Library,
}
impl CkbNativeSimulatorLib {
    pub fn new(path: &str) -> Self {
        unsafe {
            let lib = libloading::Library::new(path).expect("Load library");
            Self { lib }
        }
    }

    pub fn ckb_std_main(&self, argc: c_int, argv: *const *const i8) -> i8 {
        type CkbMainFunc<'a> =
            libloading::Symbol<'a, unsafe extern "C" fn(argc: i32, argv: *const *const i8) -> i8>;

        unsafe {
            let func: CkbMainFunc = self
                .lib
                .get(b"__ckb_std_main")
                .expect("load function : __ckb_std_main");
            func(argc as i32, argv)
        }
    }

    pub fn update_spawn_info(&self, pid: VmId) {
        type UpdateSpawnInfo<'a> =
            libloading::Symbol<'a, unsafe extern "C" fn(ptr: *mut c_void, pid: u64)>;
        use crate::spawn::*;
        use std::sync::Mutex;

        let infos_ref: &Mutex<ProcessInfos> = &*PROCESS_INFOS;
        let global_variable_ptr: *const Mutex<ProcessInfos> = infos_ref;

        unsafe {
            let func: UpdateSpawnInfo = self
                .lib
                .get(b"__update_spawn_info")
                .expect("load function : __update_spawn_info");
            func(global_variable_ptr as *mut c_void, pid)
        }
    }
}

#[derive(Clone, Copy)]
pub enum EventType {
    Unknow,
    ProcessExit((VmId, i8)), // vmid, rc
}

pub struct Event(std::sync::Arc<(std::sync::Mutex<EventType>, std::sync::Condvar)>);
impl Event {
    pub fn wait<F>(&self, condition: F)
    where
        F: Fn(EventType) -> bool + Send + Sync + 'static,
    {
        let (lock, cvar) = &*self.0;
        let mut started = lock.lock().unwrap();

        loop {
            if condition(*started) {
                break;
            }
            started = cvar.wait(started).unwrap();
        }
    }

    pub fn notify(&mut self, v: EventType) {
        let (lock, cvar) = &*self.0;
        let mut started = lock.lock().unwrap();
        *started = v;
        cvar.notify_all();
    }
}
impl Clone for Event {
    fn clone(&self) -> Self {
        Self {
            0: std::sync::Arc::clone(&self.0),
        }
    }
}
impl Default for Event {
    fn default() -> Self {
        Self {
            0: std::sync::Arc::new((
                std::sync::Mutex::new(EventType::Unknow),
                std::sync::Condvar::new(),
            )),
        }
    }
}

// copy from ckb/script/src/types.rs
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fd(pub u64);

impl Fd {
    pub fn create(slot: u64) -> (Fd, Fd, u64) {
        (Fd(slot), Fd(slot + 1), slot + 2)
    }

    pub fn _other_fd(&self) -> Fd {
        Fd(self.0 ^ 0x1)
    }

    pub fn _is_read(&self) -> bool {
        self.0 % 2 == 0
    }

    pub fn _is_write(&self) -> bool {
        self.0 % 2 == 1
    }
}
impl From<u64> for Fd {
    fn from(value: u64) -> Self {
        Self { 0: value }
    }
}
impl Into<u64> for Fd {
    fn into(self) -> u64 {
        self.0
    }
}
