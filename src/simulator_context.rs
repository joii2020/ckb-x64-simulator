use crate::{
    global_data::GlobalData,
    utils::{Event, Fd, ProcID, SimID},
};
use std::{cell::RefCell, collections::HashMap, thread::JoinHandle};

thread_local! {
    static TX_CONTEXT_ID: RefCell<SimID> = RefCell::new(SimID::default());
    static PROC_CONTEXT_ID: RefCell<ProcID> = RefCell::new(ProcID::default());
}

const MAX_PROCESSES_COUNT: u64 = 16;

#[derive(PartialEq, Eq, Debug)]
pub enum ProcStatus {
    Loaded,
    // Running,
    WaitSpawn,
    ReadWait(Fd, usize, Vec<u8>, u64),
    WriteWait(Fd, Vec<u8>, u64),
    Terminated,
}
impl Default for ProcStatus {
    fn default() -> Self {
        Self::Loaded
    }
}
impl ProcStatus {
    fn read_wait(&self) -> Option<(&Fd, &usize, &[u8])> {
        if let Self::ReadWait(fd, len, buf, _) = self {
            Some((fd, len, buf))
        } else {
            None
        }
    }
    fn read_wait_mut(&mut self) -> Option<(&mut Fd, &mut usize, &mut Vec<u8>)> {
        if let Self::ReadWait(fd, len, buf, _) = self {
            Some((fd, len, buf))
        } else {
            None
        }
    }
    fn write_wait(&self) -> Option<(&Fd, &[u8])> {
        if let Self::WriteWait(fd, buf, _) = self {
            Some((fd, buf))
        } else {
            None
        }
    }
    fn write_wait_mut(&mut self) -> Option<(&mut Fd, &mut Vec<u8>)> {
        if let Self::WriteWait(fd, buf, _) = self {
            Some((fd, buf))
        } else {
            None
        }
    }
}

#[derive(Default)]
struct ProcInfo {
    parent_id: ProcID,

    inherited_fds: Vec<Fd>,

    scheduler_event: Event,
    join_handle: Option<JoinHandle<i8>>,
}
impl ProcInfo {
    fn set_pid(id: ProcID) {
        PROC_CONTEXT_ID.with(|f| *f.borrow_mut() = id);
    }
    pub fn id() -> ProcID {
        PROC_CONTEXT_ID.with(|f| f.borrow().clone())
    }
}

pub struct SimContext {
    fds_count: u64,
    proc_id_count: ProcID,

    proc_info: HashMap<ProcID, ProcInfo>,
    process_status: HashMap<ProcID, ProcStatus>,

    fds: HashMap<Fd, ProcID>,

    dbg_status_count: u64,
}
impl Default for SimContext {
    fn default() -> Self {
        ProcInfo::set_pid(0.into());
        Self {
            fds_count: 2,
            proc_id_count: 1.into(),
            proc_info: [(0.into(), ProcInfo::default())].into(),
            process_status: Default::default(),
            fds: Default::default(),

            dbg_status_count: 0,
        }
    }
}
impl SimContext {
    pub fn update_ctx_id(id: SimID, pid: Option<ProcID>) {
        TX_CONTEXT_ID.with(|f| *f.borrow_mut() = id);
        if let Some(pid) = pid {
            ProcInfo::set_pid(pid);
        }
    }
    pub fn ctx_id() -> SimID {
        TX_CONTEXT_ID.with(|f| f.borrow().clone())
    }
    pub fn clean() {
        TX_CONTEXT_ID.with(|f| *f.borrow_mut() = 0.into());
        PROC_CONTEXT_ID.with(|f| *f.borrow_mut() = 0.into());
    }

    pub fn start_process<F: Send + 'static + FnOnce(SimID, ProcID) -> i8>(
        &mut self,
        fds: &[Fd],
        func: F,
    ) -> ProcID {
        let parent_id = ProcInfo::id();
        let id = self.proc_id_count.next();
        let process = ProcInfo {
            parent_id: parent_id.clone(),
            inherited_fds: fds.to_vec(),
            ..Default::default()
        };

        self.proc_info.insert(id.clone(), process);
        let ctx_id = SimContext::ctx_id();

        fds.iter().all(|fd| {
            self.move_pipe(fd, id.clone());
            true
        });
        self.process_status.insert(parent_id, ProcStatus::WaitSpawn);

        let id2 = id.clone();
        let join_handle = std::thread::spawn(move || {
            SimContext::update_ctx_id(ctx_id.clone(), Some(id.clone()));
            let code = func(ctx_id.clone(), id.clone());

            let mut gd = GlobalData::locked();
            let cur_sim = gd.get_tx_mut(&SimContext::ctx_id());
            cur_sim.close_all(&id);
            cur_sim.process_io();

            code
        });

        self.process_mut(&id2).join_handle = Some(join_handle);

        id2
    }
    pub fn pid() -> ProcID {
        ProcInfo::id()
    }
    pub fn inherited_fds(&self) -> Vec<Fd> {
        let process = self.process(&ProcInfo::id());
        process.inherited_fds.clone()
    }
    fn process(&self, id: &ProcID) -> &ProcInfo {
        self.proc_info
            .get(id)
            .unwrap_or_else(|| panic!("unknow process id: {:?}", id))
    }
    fn process_mut(&mut self, id: &ProcID) -> &mut ProcInfo {
        self.proc_info
            .get_mut(id)
            .unwrap_or_else(|| panic!("unknow process id: {:?}", id))
    }
    pub fn max_proc_spawned(&self) -> bool {
        u64::from(self.proc_id_count.clone()) > MAX_PROCESSES_COUNT
    }
    pub fn has_proc(&self, id: &ProcID) -> bool {
        self.proc_info.contains_key(id)
    }
    pub fn get_event(&self) -> Event {
        self.process(&ProcInfo::id()).scheduler_event.clone()
    }
    pub fn exit(&mut self, id: &ProcID) -> Option<JoinHandle<i8>> {
        let process = self.process_mut(id);

        process.join_handle.take()
    }

    fn process_io(&mut self) {
        if let Some(id) = self
            .process_status
            .iter()
            .find(|(_pid, status)| status == &&ProcStatus::Terminated)
            .map(|(id, _)| id.clone())
        {
            let p = &self.process(&id).parent_id;
            self.process(p).scheduler_event.notify();

            self.process_status.remove(&id);
            return;
        }
        if let Some(it) = self
            .process_status
            .iter()
            .find(|(_, status)| {
                if let Some((_fd, buf)) = status.write_wait() {
                    buf.is_empty()
                } else {
                    false
                }
            })
            .map(|f| f.0.clone())
        {
            self.process(&it).scheduler_event.notify();
            self.process_status.remove(&it);
            return;
        }

        let mut update_fds = Vec::new();
        self.process_status.iter().all(|(pid, status)| {
            if let Some((rfd, _len, _buf)) = status.read_wait() {
                let write_fd = rfd.other_fd();
                if let Some(w_pid) = self.fds.get(&write_fd) {
                    if let Some((w_fd, w_buf)) =
                        self.process_status.get(w_pid).and_then(|f| f.write_wait())
                    {
                        update_fds
                            .push((pid.clone(), (w_pid.clone(), w_fd.clone(), w_buf.to_vec())));
                    }
                }
            }
            true
        });
        if update_fds.iter().any(|(r_pid, (w_pid, _wfd, wbuf))| {
            let (_rfd, rlen, rbuf) = self
                .process_status
                .get_mut(r_pid)
                .and_then(|status| status.read_wait_mut())
                .unwrap();

            let copy_len = (*rlen).min(wbuf.len());
            rbuf.extend_from_slice(&wbuf[..copy_len]);
            *rlen -= copy_len;
            let rlen = rlen.clone();

            let (_, wbuf) = self
                .process_status
                .get_mut(w_pid)
                .and_then(|status| status.write_wait_mut())
                .unwrap();

            *wbuf = wbuf[copy_len..].to_vec();
            if rlen == 0 {
                self.process(r_pid).scheduler_event.notify();
                true
            } else {
                false
            }
        }) {
            return;
        }

        if let Some(it) = self
            .process_status
            .iter()
            .find(|(_, status)| {
                if let Some((_fd, buf)) = status.write_wait() {
                    buf.is_empty()
                } else {
                    false
                }
            })
            .map(|f| f.0.clone())
        {
            self.process(&it).scheduler_event.notify();
            self.process_status.remove(&it);
            return;
        }
        if let Some(it) = self
            .process_status
            .iter()
            .find(|(_, status)| {
                if let Some((_, r_len, _r_buf)) = status.read_wait() {
                    r_len == &0
                } else {
                    false
                }
            })
            .map(|f| f.0)
        {
            self.process(it).scheduler_event.notify();
            return;
        }
        if let Some(id) = self
            .process_status
            .iter()
            .find(|(_, status)| *status == &ProcStatus::WaitSpawn)
            .map(|f| f.0)
        {
            self.process(id).scheduler_event.notify();
        }
    }
    pub fn wait_read(&mut self, fd: Fd, len: usize) -> Event {
        let id = ProcInfo::id();
        let dbg_id = self.dbg_status_count;
        self.dbg_status_count += 1;

        self.process_status
            .insert(id, ProcStatus::ReadWait(fd, len, Vec::new(), dbg_id));

        println!("==wait read1 status: {:?}", self.process_status);
        self.process_io();
        println!("==wait read2 status: {:?}", self.process_status);
        self.get_event()
    }
    pub fn wait_write(&mut self, fd: Fd, buf: &[u8]) -> Event {
        let id = ProcInfo::id();
        let dbg_id = self.dbg_status_count;
        self.dbg_status_count += 1;

        self.process_status
            .insert(id, ProcStatus::WriteWait(fd, buf.to_vec(), dbg_id));

        println!("==wait write1 status: {:?}", self.process_status);
        self.process_io();
        println!("==wait write2 status: {:?}", self.process_status);

        self.get_event()
    }
    pub fn read_cache(&mut self, fd: &Fd) -> Vec<u8> {
        let mut ref_buf = Vec::new();
        if let Some((pid, buf)) = self
            .process_status
            .iter()
            .find(|(_, status)| {
                if let Some((rfd, _, _buf)) = status.read_wait() {
                    fd == rfd
                } else {
                    false
                }
            })
            .and_then(|f| f.1.read_wait().map(|p| (f.0.clone(), p.2.to_vec())))
        {
            ref_buf = buf.to_vec();
            self.process_status.remove(&pid);
        }

        ref_buf
    }

    pub fn new_pipe(&mut self) -> (Fd, Fd) {
        let pid = ProcInfo::id();
        let fds = Fd::create(self.fds_count);

        self.fds.insert(fds.0.clone(), pid.clone());
        self.fds.insert(fds.1.clone(), pid.clone());
        self.fds_count = fds.2;

        (fds.0, fds.1)
    }
    pub fn close_pipe(&mut self, fd: Fd) -> bool {
        if !self.has_fd(&fd) {
            false
        } else {
            self.fds.remove(&fd).is_some()
        }
    }
    pub fn len_pipe(&self) -> usize {
        self.fds.len()
    }
    fn move_pipe(&mut self, fd: &Fd, pid: ProcID) {
        let f = self
            .fds
            .get_mut(fd)
            .unwrap_or_else(|| panic!("unknow fd: {:?}", fd));
        *f = pid;
    }
    fn close_all(&mut self, id: &ProcID) {
        let keys_to_rm: Vec<Fd> = self
            .fds
            .iter()
            .filter(|(_k, v)| v == &id)
            .map(|(k, _v)| k.clone())
            .collect();
        for k in keys_to_rm {
            self.fds.remove(&k);
        }

        self.process_status
            .insert(id.clone(), ProcStatus::Terminated);
    }

    pub fn has_fd(&self, fd: &Fd) -> bool {
        if let Some(pid) = self.fds.get(fd) {
            &ProcInfo::id() == pid
        } else {
            false
        }
    }
    pub fn chech_other_fd(&self, fd: &Fd) -> bool {
        self.fds.contains_key(&fd.other_fd())
    }
}
