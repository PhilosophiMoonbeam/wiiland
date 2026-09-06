//! Exercise the real supervisor and scheduling loop without HID or real time.
use super::*;
use crate::uinput::RecordingBackend;
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::RawFd;

#[derive(Debug, PartialEq, Eq)]
enum Operation {
    Open(String),
    Drain(String),
    Drop(String),
    Tick(String),
    Monitor(usize),
}

struct PollStep {
    advance: Duration,
    ready: Vec<RawFd>,
    interrupted: bool,
}

struct Probe {
    now: Cell<Instant>,
    operations: RefCell<Vec<Operation>>,
    next_fd: Cell<RawFd>,
    requests: Cell<usize>,
    snapshots: RefCell<VecDeque<io::Result<Vec<PathBuf>>>>,
    steps: RefCell<VecDeque<PollStep>>,
    events: RefCell<HashMap<PathBuf, VecDeque<Result<BridgeAction, i32>>>>,
    setup_duration: Cell<Duration>,
    drain_duration: Cell<Duration>,
    monitor_pending: Cell<bool>,
}

impl Probe {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            now: Cell::new(Instant::now()),
            operations: RefCell::new(Vec::new()),
            next_fd: Cell::new(10),
            requests: Cell::new(0),
            snapshots: RefCell::new(VecDeque::new()),
            steps: RefCell::new(VecDeque::new()),
            events: RefCell::new(HashMap::new()),
            setup_duration: Cell::new(Duration::ZERO),
            drain_duration: Cell::new(Duration::ZERO),
            monitor_pending: Cell::new(false),
        })
    }
    fn snapshot(&self, paths: &[&str]) {
        self.snapshots
            .borrow_mut()
            .push_back(Ok(paths.iter().map(PathBuf::from).collect()));
    }
    fn step(&self, advance: Duration, ready: &[RawFd]) {
        self.steps.borrow_mut().push_back(PollStep {
            advance,
            ready: ready.to_vec(),
            interrupted: false,
        });
    }
    fn advance(&self, duration: Duration) {
        self.now.set(self.now.get() + duration);
    }
}

struct FakeDevice {
    path: PathBuf,
    fd: RawFd,
    probe: Rc<Probe>,
    actions: VecDeque<Result<BridgeAction, i32>>,
}

impl DeviceSession for FakeDevice {
    fn path(&self) -> &Path {
        &self.path
    }
    fn fd(&self) -> RawFd {
        self.fd
    }
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            syspath: self.path.to_string_lossy().into_owned(),
            profile: wiiland_ipc::Profile::Desktop,
            opened_interfaces: 1,
            pending_interfaces: 0,
            gamepad_output: false,
            desktop_output: true,
        }
    }
    fn drain(&mut self, _: &mut dyn FnMut(&Event)) -> Result<BridgeAction, i32> {
        self.probe
            .operations
            .borrow_mut()
            .push(Operation::Drain(self.path.display().to_string()));
        self.probe.advance(self.probe.drain_duration.get());
        self.actions
            .pop_front()
            .unwrap_or(Ok(BridgeAction::Continue))
    }
    fn set_capture(&mut self, _: bool) -> Result<(), i32> {
        Ok(())
    }
    fn pointer_active(&self) -> bool {
        true
    }
    fn tick_pointer(&mut self) -> Result<(), i32> {
        self.probe
            .operations
            .borrow_mut()
            .push(Operation::Tick(self.path.display().to_string()));
        Ok(())
    }
    fn set_trace(
        &mut self,
        _: wiiland_core::TraceFilter,
        _: Rc<Cell<u64>>,
        _: Box<dyn FnMut(&str)>,
    ) {
    }
}

impl Drop for FakeDevice {
    fn drop(&mut self) {
        self.probe
            .operations
            .borrow_mut()
            .push(Operation::Drop(self.path.display().to_string()));
    }
}

struct FakePlatform {
    probe: Rc<Probe>,
    stopped: bool,
}

impl RuntimePlatform<RecordingBackend> for FakePlatform {
    type Device = FakeDevice;
    fn open_device(
        &mut self,
        path: &Path,
        _: &Config,
        _: RecordingBackend,
        _: bool,
    ) -> Result<FakeDevice, i32> {
        self.probe
            .operations
            .borrow_mut()
            .push(Operation::Open(path.display().to_string()));
        self.probe.advance(self.probe.setup_duration.get());
        let fd = self.probe.next_fd.get();
        self.probe.next_fd.set(fd + 1);
        let actions = self
            .probe
            .events
            .borrow_mut()
            .remove(path)
            .unwrap_or_default();
        Ok(FakeDevice {
            path: path.to_path_buf(),
            fd,
            probe: self.probe.clone(),
            actions,
        })
    }
    fn start_monitor(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn monitor_fd(&self) -> Option<RawFd> {
        Some(2)
    }
    fn drain_monitor(&mut self, budget: usize) -> io::Result<bool> {
        self.probe
            .operations
            .borrow_mut()
            .push(Operation::Monitor(budget));
        Ok(self.probe.monitor_pending.replace(false))
    }
    fn request_snapshot(&mut self) -> io::Result<()> {
        self.probe.requests.set(self.probe.requests.get() + 1);
        Ok(())
    }
    fn take_snapshot(&mut self) -> Option<io::Result<Vec<PathBuf>>> {
        self.probe.snapshots.borrow_mut().pop_front()
    }
    fn signal_fd(&self) -> RawFd {
        1
    }
    fn shutdown_requested(&self) -> bool {
        self.stopped
    }
    fn drain_signal(&mut self) {}
    fn now(&self) -> Instant {
        self.probe.now.get()
    }
    fn poll(&mut self, fds: &mut [libc::pollfd], _: i32) -> io::Result<usize> {
        let Some(step) = self.probe.steps.borrow_mut().pop_front() else {
            self.stopped = true;
            return Ok(0);
        };
        self.probe.advance(step.advance);
        if step.interrupted {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let mut ready = 0;
        for fd in fds {
            if step.ready.contains(&fd.fd) {
                fd.revents = libc::POLLIN;
                ready += 1;
            }
        }
        Ok(ready)
    }
}

fn runtime(probe: &Rc<Probe>) -> Runtime<RecordingBackend, FakePlatform> {
    Runtime::with_platform(
        Config::default(),
        RecordingBackend::new(),
        FakePlatform {
            probe: probe.clone(),
            stopped: false,
        },
    )
    .unwrap()
}

#[test]
fn shutdown_readiness_precedes_other_ready_devices() {
    let probe = Probe::new();
    let mut runtime = runtime(&probe);
    runtime.add_path("/active").unwrap();
    probe.step(Duration::ZERO, &[1, 10]);
    assert!(runtime.poll_once(0, false).unwrap());
    assert_eq!(
        *probe.operations.borrow(),
        [Operation::Open("/active".into())]
    );
}

#[test]
fn ready_owners_are_drained_before_discovery_reuses_their_slots() {
    let probe = Probe::new();
    let mut runtime = runtime(&probe);
    runtime.add_path("/old").unwrap();
    probe.step(Duration::ZERO, &[10]);
    probe.snapshot(&["/new"]);
    runtime.request_reconcile();
    assert!(!runtime.poll_once(0, false).unwrap());
    runtime.service_discovery();
    assert_eq!(
        *probe.operations.borrow(),
        [
            Operation::Open("/old".into()),
            Operation::Drain("/old".into()),
            Operation::Drop("/old".into()),
            Operation::Open("/new".into()),
        ]
    );
    assert_eq!(runtime.slots[0].as_ref().unwrap().path(), Path::new("/new"));
}

#[test]
fn failed_snapshot_preserves_owned_devices_and_coalesces_retry_requests() {
    let probe = Probe::new();
    let mut runtime = runtime(&probe);
    runtime.add_path("/retained").unwrap();
    runtime.request_reconcile();
    for _ in 0..20 {
        runtime.request_reconcile();
    }
    assert_eq!(probe.requests.get(), 1);
    probe
        .snapshots
        .borrow_mut()
        .push_back(Err(io::Error::from_raw_os_error(libc::EIO)));
    runtime.service_discovery();
    assert!(runtime.find(Path::new("/retained")).is_some());
    assert_eq!(probe.requests.get(), 2);
    assert_eq!(probe.operations.borrow().len(), 1);
}

#[test]
fn discovery_setup_yields_to_ready_input_and_pointer_ticks() {
    let probe = Probe::new();
    let mut runtime = runtime(&probe);
    probe.snapshot(&["/first", "/second", "/third"]);
    probe.step(Duration::ZERO, &[]);
    probe.step(POINTER_TICK, &[10, 11]);
    runtime.run_monitor().unwrap();
    let operations = probe.operations.borrow();
    let third = operations
        .iter()
        .position(|op| *op == Operation::Open("/third".into()))
        .unwrap();
    assert_eq!(
        operations[..third]
            .iter()
            .filter(|op| matches!(op, Operation::Open(_)))
            .count(),
        DEVICE_SETUP_BUDGET
    );
    assert!(operations[..third].contains(&Operation::Drain("/first".into())));
    assert!(operations[..third].contains(&Operation::Drain("/second".into())));
    assert!(operations[..third].contains(&Operation::Tick("/first".into())));
    assert!(operations[..third].contains(&Operation::Tick("/second".into())));
}

#[test]
fn dispatch_metrics_include_periodic_discovery_and_device_work() {
    let probe = Probe::new();
    let mut runtime = runtime(&probe);
    runtime.add_path("/first").unwrap();
    probe.setup_duration.set(Duration::from_millis(3));
    probe.drain_duration.set(Duration::from_millis(2));
    probe.snapshot(&["/first", "/second"]);
    probe.step(RECONCILE_TICK, &[10]);
    runtime.loop_run(false).unwrap();
    assert_eq!(runtime.metrics.max_dispatch_duration_us, 5_000);
    assert_eq!(probe.requests.get(), 1);
}

#[test]
fn interrupted_poll_does_not_count_waiting_as_dispatch_work() {
    let probe = Probe::new();
    let mut runtime = runtime(&probe);
    probe.steps.borrow_mut().push_back(PollStep {
        advance: Duration::from_millis(200),
        ready: Vec::new(),
        interrupted: true,
    });
    runtime.loop_run(false).unwrap();
    assert_eq!(runtime.metrics.max_dispatch_duration_us, 0);
}

#[test]
fn device_failure_is_local_in_monitor_mode_and_fatal_in_single_mode() {
    for single in [false, true] {
        let probe = Probe::new();
        probe
            .events
            .borrow_mut()
            .insert(PathBuf::from("/failed"), VecDeque::from([Err(-libc::EIO)]));
        let mut runtime = runtime(&probe);
        runtime.add_path("/failed").unwrap();
        runtime.add_path("/healthy").unwrap();
        probe.step(Duration::ZERO, &[10, 11]);
        let outcome = runtime.poll_once(0, single);
        assert!(runtime.find(Path::new("/failed")).is_none());
        assert!(runtime.find(Path::new("/healthy")).is_some());
        if single {
            assert_eq!(outcome, Err(-libc::EIO));
        } else {
            assert_eq!(outcome, Ok(false));
            assert!(
                probe
                    .operations
                    .borrow()
                    .contains(&Operation::Drain("/healthy".into()))
            );
        }
    }
}

#[test]
fn pending_monitor_work_continues_without_another_fd_notification() {
    let probe = Probe::new();
    let mut runtime = runtime(&probe);
    probe.monitor_pending.set(true);
    probe.step(Duration::ZERO, &[2]);
    probe.step(Duration::ZERO, &[]);
    assert!(!runtime.poll_once(0, false).unwrap());
    assert!(runtime.monitor_pending);
    assert!(!runtime.poll_once(0, false).unwrap());
    assert!(!runtime.monitor_pending);
    assert_eq!(
        *probe.operations.borrow(),
        [
            Operation::Monitor(MONITOR_EVENT_BUDGET),
            Operation::Monitor(MONITOR_EVENT_BUDGET)
        ]
    );
    assert_eq!(probe.requests.get(), 1);
}
