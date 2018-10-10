extern crate epoll;
extern crate libc;
extern crate time;
extern crate timerfd;

extern crate api_server;
extern crate data_model;
extern crate devices;
extern crate kernel_loader;
extern crate kvm;
#[macro_use]
extern crate logger;
extern crate memory_model;
extern crate net_util;
extern crate seccomp;
extern crate sys_util;
extern crate x86_64;

pub mod default_syscalls;
mod device_config;
mod device_manager;
pub mod kernel_cmdline;
mod vm_control;
mod vstate;

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{metadata, File, OpenOptions};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::result;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, ATOMIC_USIZE_INIT};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Barrier, RwLock};
use std::thread;
use std::time::Duration;

use libc::{c_void, siginfo_t};
use timerfd::{ClockId, SetTimeFlags, TimerFd, TimerState};

use api_server::request::boot_source::BootSourceConfigError;
use api_server::request::instance_info::{InstanceInfo, InstanceState};
use api_server::request::logger::{APILoggerDescription, APILoggerError, APILoggerLevel};
use api_server::request::machine_configuration::PutMachineConfigurationError;
use api_server::request::net::{NetworkInterfaceBody, NetworkInterfaceError};
use api_server::request::{
    Error as SyncError, ErrorType, GenerateResponse, OutcomeSender, VmmAction,
};
use data_model::vm::{
    description_into_implementation as rate_limiter_description_into_implementation,
    BlockDeviceConfig, BlockDeviceConfigs, DriveError, MachineConfiguration,
};
use device_config::*;
use device_manager::legacy::LegacyDeviceManager;
use device_manager::mmio::MMIODeviceManager;
use devices::virtio;
use devices::{DeviceEventT, EpollHandler, EpollHandlerPayload};
use kvm::*;
use logger::{Level, Metric, LOGGER, METRICS};
use memory_model::{GuestAddress, GuestMemory, GuestMemoryError};
use sys_util::{register_signal_handler, EventFd, Killable, Terminal};
use vm_control::VmResponse;
use vstate::{Vcpu, Vm};

const MAGIC_IOPORT_SIGNAL_GUEST_BOOT_COMPLETE: u16 = 0x03f0;
const MAGIC_VALUE_SIGNAL_GUEST_BOOT_COMPLETE: u8 = 123;

const DEFAULT_KERNEL_CMDLINE: &str = "reboot=k panic=1 pci=off nomodules 8250.nr_uarts=0";
const VCPU_RTSIG_OFFSET: i32 = 0;
const WRITE_METRICS_PERIOD_SECONDS: u64 = 60;
static START_INSTANCE_REQUEST_TS: AtomicUsize = ATOMIC_USIZE_INIT;

/// User Errors describe bad configuration (user input).
#[derive(Debug)]
pub enum UserError {
    /// This error is thrown by the minimal boot loader implementation.
    /// It is related to a faulty memory configuration.
    ConfigureSystem(x86_64::Error),
    /// Unable to seek the block device backing file due to invalid permissions or
    /// the file was deleted/corrupted.
    CreateBlockDevice(sys_util::Error),
    /// This error can come from both bad user input and internal errors and we should probably
    /// split this at some point.
    /// Internal errors are due to resource exhaustion.
    /// Users errors  are due to invalid permissions.
    CreateNetDevice(devices::virtio::Error),
    /// This error describes a bad user configuration by which a VM can end up with two
    /// root block devices. Invalid operations on drives will also return a Drive Error.
    Drive(DriveError),
    /// The kernel path is invalid.
    InvalidKernelPath,
    /// The start command was issued more than once.
    MicroVMAlreadyRunning,
    /// The kernel command line is invalid.
    KernelCmdLine(kernel_cmdline::Error),
    /// Cannot load kernel due to invalid memory configuration or invalid kernel image.
    KernelLoader(kernel_loader::Error),
    /// Cannot open /dev/kvm. Either the host does not have KVM or Firecracker does not have
    /// permission to open the file descriptor.
    Kvm(sys_util::Error),
    /// The host kernel reports an invalid KVM API version.
    KvmApiVersion(i32),
    /// Cannot initialize the KVM context due to missing capabilities.
    KvmCap(kvm::Cap),
    /// Cannot start the VM because the kernel was not configured.
    MissingKernelConfig,
    /// Cannot open the block device backing file.
    OpenBlockDevice(std::io::Error),
}

/// These errors are unrelated to the user and usually refer to logical errors
/// or bad management of resources (memory, file descriptors & others).
#[derive(Debug)]
pub enum InternalError {
    ApiChannel,
    /// Creating a Rate Limiter can fail because of resource exhaustion when trying to
    /// create a new timer file descriptor.
    CreateRateLimiter(std::io::Error),
    /// Legacy devices work with Event file descriptors and the creation can fail because
    /// of resource exhaustion.
    CreateLegacyDevice(device_manager::legacy::Error),
    /// Executing a VM request failed.
    DeviceVmRequest(sys_util::Error),
    /// An operation on the epoll instance failed due to resource exhaustion or bad configuration.
    EpollFd(std::io::Error),
    /// Cannot read from an Event file descriptor.
    EventFd(sys_util::Error),
    /// Describes a logical problem.
    GeneralFailure, // TODO: there are some cases in which this error should be replaced.
    /// Memory regions are overlapping or mmap fails.
    GuestMemory(GuestMemoryError),
    /// Cannot add devices to the Legacy I/O Bus.
    LegacyIOBus(device_manager::legacy::Error),
    /// The net device configuration is missing the tap device.
    NetDeviceUnconfigured,
    /// Epoll wait failed.
    Poll(std::io::Error),
    /// Cannot initialize a MMIO Block Device or add a device to the MMIO Bus.
    RegisterBlockDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Network Device or add a device to the MMIO Bus.
    RegisterNetDevice(device_manager::mmio::Error),
    /// Write to the serial console failed.
    Serial(sys_util::Error),
    /// Cannot initialize/configure the STDIN handle.
    StdinHandle(sys_util::Error),
    /// Cannot create Timer file descriptor.
    TimerFd(std::io::Error),
    /// Cannot create a new vCPU file descriptor.
    Vcpu(vstate::Error),
    /// vCPU configuration failed.
    VcpuConfigure(vstate::Error),
    /// Cannot spawn a new vCPU thread.
    VcpuSpawn(std::io::Error),
    /// Cannot open the VM file descriptor.
    Vm(vstate::Error),
    /// Cannot configure the VM.
    VmConfigure(vstate::Error),
}

#[derive(Debug)]
pub enum Error {
    User(UserError),
    Internal(InternalError),
}

impl std::convert::From<InternalError> for Error {
    fn from(err: InternalError) -> Self {
        Error::Internal(err)
    }
}

impl std::convert::From<UserError> for Error {
    fn from(err: UserError) -> Self {
        Error::User(err)
    }
}

impl std::convert::From<kernel_loader::Error> for Error {
    fn from(e: kernel_loader::Error) -> Error {
        Error::User(UserError::KernelLoader(e))
    }
}

impl std::convert::From<x86_64::Error> for Error {
    fn from(e: x86_64::Error) -> Error {
        Error::User(UserError::ConfigureSystem(e))
    }
}

impl std::convert::From<kernel_cmdline::Error> for Error {
    fn from(e: kernel_cmdline::Error) -> Error {
        Error::Internal(InternalError::RegisterBlockDevice(
            device_manager::mmio::Error::Cmdline(e),
        ))
    }
}

type Result<T> = std::result::Result<T, Error>;

// Allows access to the functionality of the KVM wrapper only as long as every required
// KVM capability is present on the host.
struct KvmContext {
    kvm: Kvm,
    nr_vcpus: usize,
    max_vcpus: usize,
}

impl KvmContext {
    fn new() -> Result<Self> {
        fn check_cap(kvm: &Kvm, cap: Cap) -> std::result::Result<(), Error> {
            if !kvm.check_extension(cap) {
                return Err(Error::User(UserError::KvmCap(cap)));
            }
            Ok(())
        }

        let kvm = Kvm::new().map_err(UserError::Kvm)?;

        if kvm.get_api_version() != kvm::KVM_API_VERSION as i32 {
            return Err(Error::User(UserError::KvmApiVersion(kvm.get_api_version())));
        }

        check_cap(&kvm, Cap::Irqchip)?;
        check_cap(&kvm, Cap::Ioeventfd)?;
        check_cap(&kvm, Cap::Irqfd)?;
        // check_cap(&kvm, Cap::ImmediateExit)?;
        check_cap(&kvm, Cap::SetTssAddr)?;
        check_cap(&kvm, Cap::UserMemory)?;

        let nr_vcpus = kvm.get_nr_vcpus();
        let max_vcpus = match kvm.check_extension_int(Cap::MaxVcpus) {
            0 => nr_vcpus,
            x => x as usize,
        };

        Ok(KvmContext {
            kvm,
            nr_vcpus,
            max_vcpus,
        })
    }

    fn fd(&self) -> &Kvm {
        &self.kvm
    }

    #[allow(dead_code)]
    fn nr_vcpus(&self) -> usize {
        self.nr_vcpus
    }

    #[allow(dead_code)]
    fn max_vcpus(&self) -> usize {
        self.max_vcpus
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum EpollDispatch {
    Exit,
    Stdin,
    DeviceHandler(usize, DeviceEventT),
    VmmActionRequest,
    WriteMetrics,
}

struct MaybeHandler {
    handler: Option<Box<EpollHandler>>,
    receiver: Receiver<Box<EpollHandler>>,
}

impl MaybeHandler {
    fn new(receiver: Receiver<Box<EpollHandler>>) -> Self {
        MaybeHandler {
            handler: None,
            receiver,
        }
    }
}

struct EpollEvent<T: AsRawFd> {
    dispatch_index: u64,
    fd: T,
}

// Handles epoll related business.
// A glaring shortcoming of the current design is the liberal passing around of raw_fds,
// and duping of file descriptors. This issue will be solved when we also implement device removal.
struct EpollContext {
    epoll_raw_fd: RawFd,
    stdin_index: u64,
    // FIXME: find a different design as this does not scale. This Vec can only grow.
    dispatch_table: Vec<Option<EpollDispatch>>,
    device_handlers: Vec<MaybeHandler>,
}

impl EpollContext {
    fn new() -> Result<Self> {
        let epoll_raw_fd = epoll::create(true).map_err(InternalError::EpollFd)?;

        // Initial capacity needs to be large enough to hold:
        // * 1 exit event
        // * 1 stdin event
        // * 2 queue events for virtio block
        // * 4 for virtio net
        // The total is 8 elements; allowing spare capacity to avoid reallocations.
        let mut dispatch_table = Vec::with_capacity(20);
        let stdin_index = dispatch_table.len() as u64;
        dispatch_table.push(None);
        Ok(EpollContext {
            epoll_raw_fd,
            stdin_index,
            dispatch_table,
            device_handlers: Vec::with_capacity(6),
        })
    }

    pub fn enable_stdin_event(&mut self) -> Result<()> {
        if let Err(e) = epoll::ctl(
            self.epoll_raw_fd,
            epoll::EPOLL_CTL_ADD,
            libc::STDIN_FILENO,
            epoll::Event::new(epoll::EPOLLIN, self.stdin_index),
        ) {
            // TODO: We just log this message, and immediately return Ok, instead of returning the
            // actual error because this operation always fails with EPERM when adding a fd which
            // has been redirected to /dev/null via dup2 (this may happen inside the jailer).
            // Find a better solution to this (and think about the state of the serial device
            // while we're at it). This also led to commenting out parts of the
            // enable_disable_stdin_test() unit test function.
            warn!("Could not add stdin event to epoll. {:?}", e);
            return Ok(());
        }

        self.dispatch_table[self.stdin_index as usize] = Some(EpollDispatch::Stdin);

        Ok(())
    }

    fn disable_stdin_event(&mut self) -> Result<()> {
        // Ignore failure to remove from epoll. The only reason for failure is
        // that stdin has closed or changed in which case we won't get
        // any more events on the original event_fd anyway.
        let _ = epoll::ctl(
            self.epoll_raw_fd,
            epoll::EPOLL_CTL_DEL,
            libc::STDIN_FILENO,
            epoll::Event::new(epoll::EPOLLIN, self.stdin_index),
        ).map_err(InternalError::EpollFd);
        self.dispatch_table[self.stdin_index as usize] = None;

        Ok(())
    }

    fn add_event<T>(&mut self, fd: T, token: EpollDispatch) -> Result<EpollEvent<T>>
    where
        T: AsRawFd,
    {
        let dispatch_index = self.dispatch_table.len() as u64;
        epoll::ctl(
            self.epoll_raw_fd,
            epoll::EPOLL_CTL_ADD,
            fd.as_raw_fd(),
            epoll::Event::new(epoll::EPOLLIN, dispatch_index),
        ).map_err(InternalError::EpollFd)?;
        self.dispatch_table.push(Some(token));

        Ok(EpollEvent { dispatch_index, fd })
    }

    fn remove_event<T>(&mut self, epoll_event: EpollEvent<T>) -> Result<()>
    where
        T: AsRawFd,
    {
        epoll::ctl(
            self.epoll_raw_fd,
            epoll::EPOLL_CTL_DEL,
            epoll_event.fd.as_raw_fd(),
            epoll::Event::new(epoll::EPOLLIN, epoll_event.dispatch_index),
        ).map_err(InternalError::EpollFd)?;
        self.dispatch_table[epoll_event.dispatch_index as usize] = None;

        Ok(())
    }

    fn allocate_tokens(&mut self, count: usize) -> (u64, Sender<Box<EpollHandler>>) {
        let dispatch_base = self.dispatch_table.len() as u64;
        let device_idx = self.device_handlers.len();
        let (sender, receiver) = channel();

        for x in 0..count {
            self.dispatch_table.push(Some(EpollDispatch::DeviceHandler(
                device_idx,
                x as DeviceEventT,
            )));
        }

        self.device_handlers.push(MaybeHandler::new(receiver));

        (dispatch_base, sender)
    }

    fn allocate_virtio_block_tokens(&mut self) -> (virtio::block::EpollConfig, usize) {
        let (dispatch_base, sender) = self.allocate_tokens(virtio::block::BLOCK_EVENTS_COUNT);
        (
            virtio::block::EpollConfig::new(dispatch_base, self.epoll_raw_fd, sender),
            self.device_handlers.len(),
        )
    }

    fn allocate_virtio_net_tokens(&mut self) -> virtio::net::EpollConfig {
        let (dispatch_base, sender) = self.allocate_tokens(virtio::net::NET_EVENTS_COUNT);
        virtio::net::EpollConfig::new(dispatch_base, self.epoll_raw_fd, sender)
    }

    fn get_device_handler(&mut self, device_idx: usize) -> Result<&mut EpollHandler> {
        let ref mut maybe = self.device_handlers[device_idx];
        match maybe.handler {
            Some(ref mut v) => Ok(v.as_mut()),
            None => {
                // This should only be called in response to an epoll trigger.
                // Moreover, this branch of the match should only be active on the first call
                // (the first epoll event for this device), therefore the channel is guaranteed
                // to contain a message for the first epoll event since both epoll event
                // registration and channel send() happen in the device activate() function.
                let received = maybe
                    .receiver
                    .try_recv()
                    .map_err(|_| InternalError::GeneralFailure)?;
                Ok(maybe.handler.get_or_insert(received).as_mut())
            }
        }
    }
}

impl Drop for EpollContext {
    fn drop(&mut self) {
        let rc = unsafe { libc::close(self.epoll_raw_fd) };
        if rc != 0 {
            warn!("Cannot close epoll.");
        }
    }
}

pub struct KernelConfig {
    cmdline: kernel_cmdline::Cmdline,
    kernel_file: File,
    cmdline_addr: GuestAddress,
}

pub struct Vmm {
    _kvm: KvmContext,

    vm_config: MachineConfiguration,
    shared_info: Arc<RwLock<InstanceInfo>>,

    // guest VM core resources
    guest_memory: Option<GuestMemory>,
    kernel_config: Option<KernelConfig>,
    kill_signaled: Option<Arc<AtomicBool>>,
    vcpu_handles: Option<Vec<thread::JoinHandle<()>>>,
    exit_evt: Option<EpollEvent<EventFd>>,
    vm: Vm,

    // guest VM devices
    mmio_device_manager: Option<MMIODeviceManager>,
    legacy_device_manager: LegacyDeviceManager,
    drive_handler_id_map: HashMap<String, usize>,

    // If there is a Root Block Device, this should be added as the first element of the list
    // This is necessary because we want the root to always be mounted on /dev/vda
    block_device_configs: BlockDeviceConfigs,
    network_interface_configs: NetworkInterfaceConfigs,

    epoll_context: EpollContext,

    // api resources
    api_event: EpollEvent<EventFd>,
    from_api: Receiver<Box<VmmAction>>,

    write_metrics_event: EpollEvent<TimerFd>,

    // The level of seccomp filtering used. Seccomp filters are loaded before executing guest code.
    // See `seccomp::SeccompLevel` for more information about seccomp levels.
    seccomp_level: u32,
}

impl Vmm {
    fn new(
        api_shared_info: Arc<RwLock<InstanceInfo>>,
        api_event_fd: EventFd,
        from_api: Receiver<Box<VmmAction>>,
        seccomp_level: u32,
    ) -> Result<Self> {
        let mut epoll_context = EpollContext::new()?;
        // If this fails, it's fatal; using expect() to crash.
        let api_event = epoll_context
            .add_event(api_event_fd, EpollDispatch::VmmActionRequest)
            .expect("Cannot add API eventfd to epoll.");

        let write_metrics_event = epoll_context
            .add_event(
                // non-blocking & close on exec
                TimerFd::new_custom(ClockId::Monotonic, true, true)
                    .map_err(InternalError::TimerFd)?,
                EpollDispatch::WriteMetrics,
            ).expect("Cannot add write metrics TimerFd to epoll.");

        let block_device_configs = BlockDeviceConfigs::new();
        let kvm = KvmContext::new()?;
        let vm = Vm::new(kvm.fd()).map_err(InternalError::Vm)?;

        Ok(Vmm {
            _kvm: kvm,
            vm_config: MachineConfiguration::default(),
            shared_info: api_shared_info,
            guest_memory: None,
            kernel_config: None,
            kill_signaled: None,
            vcpu_handles: None,
            exit_evt: None,
            vm,
            mmio_device_manager: None,
            legacy_device_manager: LegacyDeviceManager::new()
                .map_err(InternalError::CreateLegacyDevice)?,
            block_device_configs,
            drive_handler_id_map: HashMap::new(),
            network_interface_configs: NetworkInterfaceConfigs::new(),
            epoll_context,
            api_event,
            from_api,
            write_metrics_event,
            seccomp_level,
        })
    }

    fn update_drive_handler(
        &mut self,
        drive_id: &String,
        disk_image: File,
    ) -> result::Result<(), DriveError> {
        if let Some(device_idx) = self.drive_handler_id_map.get(drive_id) {
            match self.epoll_context.get_device_handler(*device_idx) {
                Ok(handler) => {
                    handler.handle_event(
                        virtio::block::FS_UPDATE_EVENT,
                        *device_idx as u32,
                        EpollHandlerPayload::DrivePayload(disk_image),
                    );
                    Ok(())
                }
                Err(e) => {
                    warn!("invalid handler for device {}: {:?}", device_idx, e);
                    Err(DriveError::BlockDeviceUpdateFailed)
                }
            }
        } else {
            Err(DriveError::BlockDeviceUpdateFailed)
        }
    }

    // Attaches all block devices from the BlockDevicesConfig.
    fn attach_block_devices(&mut self, device_manager: &mut MMIODeviceManager) -> Result<()> {
        let block_dev = &self.block_device_configs;
        // We rely on check_health function for making sure kernel_config is not None.
        let kernel_config = self.kernel_config.as_mut().unwrap();

        if block_dev.has_root_block_device() {
            // If no PARTUUID was specified for the root device, try with the /dev/vda.
            if !block_dev.has_partuuid_root() {
                kernel_config.cmdline.insert_str(" root=/dev/vda")?;

                if block_dev.has_read_only_root() {
                    kernel_config.cmdline.insert_str(" ro")?;
                }
            }
        }

        let epoll_context = &mut self.epoll_context;
        for drive_config in self.block_device_configs.config_list.iter() {
            // Add the block device from file.
            let block_file = OpenOptions::new()
                .read(true)
                .write(!drive_config.is_read_only)
                .open(&drive_config.path_on_host)
                .map_err(UserError::OpenBlockDevice)?;

            if drive_config.is_root_device && drive_config.get_partuuid().is_some() {
                kernel_config.cmdline.insert_str(format!(
                    " root=PARTUUID={}",
                    //The unwrap is safe as we are firstly checking that partuuid is_some().
                    drive_config.get_partuuid().unwrap()
                ))?;
                if drive_config.is_read_only {
                    kernel_config.cmdline.insert_str(" ro")?;
                }
            }

            let (epoll_config, curr_device_idx) = epoll_context.allocate_virtio_block_tokens();
            self.drive_handler_id_map
                .insert(drive_config.drive_id.clone(), curr_device_idx - 1);

            let rate_limiter =
                rate_limiter_description_into_implementation(drive_config.rate_limiter.as_ref())
                    .map_err(InternalError::CreateRateLimiter)?;
            let block_box = Box::new(
                devices::virtio::Block::new(
                    block_file,
                    drive_config.is_read_only,
                    epoll_config,
                    rate_limiter,
                ).map_err(UserError::CreateBlockDevice)?,
            );
            device_manager
                .register_device(
                    block_box,
                    &mut kernel_config.cmdline,
                    Some(drive_config.drive_id.clone()),
                ).map_err(InternalError::RegisterBlockDevice)?;
        }

        Ok(())
    }

    fn attach_net_devices(&mut self, device_manager: &mut MMIODeviceManager) -> Result<()> {
        // We rely on check_health function for making sure kernel_config is not None.
        let kernel_config = self.kernel_config.as_mut().unwrap();

        for cfg in self.network_interface_configs.iter_mut() {
            let epoll_config = self.epoll_context.allocate_virtio_net_tokens();

            let rx_rate_limiter =
                rate_limiter_description_into_implementation(cfg.rx_rate_limiter.as_ref())
                    .map_err(InternalError::CreateRateLimiter)?;
            let tx_rate_limiter =
                rate_limiter_description_into_implementation(cfg.tx_rate_limiter.as_ref())
                    .map_err(InternalError::CreateRateLimiter)?;

            let allow_mmds_requests = cfg.allow_mmds_requests();

            if let Some(tap) = cfg.take_tap() {
                let net_box = Box::new(
                    devices::virtio::Net::new_with_tap(
                        tap,
                        cfg.guest_mac(),
                        epoll_config,
                        rx_rate_limiter,
                        tx_rate_limiter,
                        allow_mmds_requests,
                    ).map_err(UserError::CreateNetDevice)?,
                );

                device_manager
                    .register_device(net_box, &mut kernel_config.cmdline, None)
                    .map_err(InternalError::RegisterNetDevice)?;
            } else {
                return Err(InternalError::NetDeviceUnconfigured)?;
            }
        }
        Ok(())
    }

    fn configure_kernel(&mut self, kernel_config: KernelConfig) {
        self.kernel_config = Some(kernel_config);
    }

    fn init_guest_memory(&mut self) -> Result<()> {
        // It is safe to unwrap because vm_config it is initialized with a default value.
        let mem_size = self.vm_config.mem_size_mib.unwrap() << 20;
        let arch_mem_regions = x86_64::arch_memory_regions(mem_size);
        self.guest_memory =
            Some(GuestMemory::new(&arch_mem_regions).map_err(InternalError::GuestMemory)?);
        Ok(())
    }

    fn check_health(&self) -> Result<()> {
        if self.kernel_config.is_none() {
            return Err(UserError::MissingKernelConfig)?;
        }
        Ok(())
    }

    fn init_devices(&mut self) -> Result<()> {
        let guest_mem = self.guest_memory.clone().ok_or(InternalError::GuestMemory(
            memory_model::GuestMemoryError::MemoryNotInitialized,
        ))?;
        // Instantiate the MMIO device manager.
        // 'mmio_base' address has to be an address which is protected by the kernel, in this case
        // the start of the x86 specific gap of memory (currently hardcoded at 768MiB).
        let mut device_manager =
            MMIODeviceManager::new(guest_mem.clone(), x86_64::get_32bit_gap_start() as u64);

        self.attach_block_devices(&mut device_manager)?;
        self.attach_net_devices(&mut device_manager)?;

        self.mmio_device_manager = Some(device_manager);
        Ok(())
    }

    fn init_microvm(&mut self) -> Result<()> {
        self.vm
            .memory_init(self.guest_memory.clone().ok_or(InternalError::VmConfigure(
                vstate::Error::GuestMemory(memory_model::GuestMemoryError::MemoryNotInitialized),
            ))?).map_err(InternalError::VmConfigure)?;
        self.vm
            .setup_irqchip(
                &self.legacy_device_manager.com_evt_1_3,
                &self.legacy_device_manager.com_evt_2_4,
            ).map_err(InternalError::VmConfigure)?;
        self.vm.create_pit().map_err(InternalError::VmConfigure)?;

        // It is safe to unwrap() because mmio_device_manager is instantiated in init_devices, which
        // is called before init_microvm.
        let device_manager = self.mmio_device_manager.as_ref().unwrap();
        for request in &device_manager.vm_requests {
            if let VmResponse::Err(e) = request.execute(self.vm.get_fd()) {
                return Err(InternalError::DeviceVmRequest(e))?;
            }
        }

        self.legacy_device_manager
            .register_devices()
            .map_err(InternalError::LegacyIOBus)?;

        Ok(())
    }

    fn start_vcpus(&mut self, entry_addr: GuestAddress) -> Result<()> {
        // It is safe to unwrap because vm_config has a default value for vcpu_count.
        let vcpu_count = self.vm_config.vcpu_count.unwrap();
        self.vcpu_handles = Some(Vec::with_capacity(vcpu_count as usize));
        // It is safe to unwrap since it's set just above.
        let vcpu_handles = self.vcpu_handles.as_mut().unwrap();
        self.kill_signaled = Some(Arc::new(AtomicBool::new(false)));
        // It is safe to unwrap since it's set just above.
        let kill_signaled = self.kill_signaled.as_mut().unwrap();

        let vcpu_thread_barrier = Arc::new(Barrier::new((vcpu_count + 1) as usize));

        for cpu_id in 0..vcpu_count {
            let io_bus = self.legacy_device_manager.io_bus.clone();
            // It is safe to unwrap() because mmio_device_manager is instantiated in init_devices,
            // which is called before start_vcpus.
            let device_manager = self.mmio_device_manager.as_ref().unwrap();
            let mmio_bus = device_manager.bus.clone();
            let kill_signaled = kill_signaled.clone();
            let vcpu_thread_barrier = vcpu_thread_barrier.clone();
            // If the lock is poisoned, it's OK to panic.
            let vcpu_exit_evt = self
                .legacy_device_manager
                .i8042
                .lock()
                .unwrap()
                .get_eventfd_clone()
                .map_err(|_| InternalError::GeneralFailure)?;

            let mut vcpu = Vcpu::new(cpu_id, &self.vm).map_err(InternalError::Vcpu)?;

            // It is safe to unwrap the ht_enabled flag because the machine configure
            // has default values for all fields.
            vcpu.configure(&self.vm_config, entry_addr, &self.vm)
                .map_err(InternalError::VcpuConfigure)?;
            vcpu_handles.push(
                thread::Builder::new()
                    .name(format!("fc_vcpu{}", cpu_id))
                    .spawn(move || {
                        unsafe {
                            extern "C" fn handle_signal(_: i32, _: *mut siginfo_t, _: *mut c_void) {
                            }
                            // This uses an async signal safe handler to kill the vcpu handles.
                            register_signal_handler(
                                VCPU_RTSIG_OFFSET,
                                sys_util::SignalHandler::Siginfo(handle_signal),
                                true,
                            ).expect("Failed to register vcpu signal handler");
                        }

                        vcpu_thread_barrier.wait();

                        loop {
                            match vcpu.run() {
                                Ok(run) => match run {
                                    VcpuExit::IoIn(addr, data) => {
                                        io_bus.read(addr as u64, data);
                                        METRICS.vcpu.exit_io_in.inc();
                                    }
                                    VcpuExit::IoOut(addr, data) => {
                                        if addr == MAGIC_IOPORT_SIGNAL_GUEST_BOOT_COMPLETE
                                            && data[0] == MAGIC_VALUE_SIGNAL_GUEST_BOOT_COMPLETE
                                        {
                                            let boot_time_ns = time::precise_time_ns() as usize
                                                - START_INSTANCE_REQUEST_TS.load(Ordering::Acquire);
                                            warn!(
                                                "Guest-boot-time = {:>6} us {} ms",
                                                boot_time_ns / 1000,
                                                boot_time_ns / 1000000
                                            );
                                        }
                                        io_bus.write(addr as u64, data);
                                        METRICS.vcpu.exit_io_out.inc();
                                    }
                                    VcpuExit::MmioRead(addr, data) => {
                                        mmio_bus.read(addr, data);
                                        METRICS.vcpu.exit_mmio_read.inc();
                                    }
                                    VcpuExit::MmioWrite(addr, data) => {
                                        mmio_bus.write(addr, data);
                                        METRICS.vcpu.exit_mmio_write.inc();
                                    }
                                    VcpuExit::Hlt => {
                                        info!("Received KVM_EXIT_HLT signal");
                                        break;
                                    }
                                    VcpuExit::Shutdown => {
                                        info!("Received KVM_EXIT_SHUTDOWN signal");
                                        break;
                                    }
                                    // Documentation specifies that below kvm exits are considered
                                    // errors.
                                    VcpuExit::FailEntry => {
                                        METRICS.vcpu.failures.inc();
                                        error!("Received KVM_EXIT_FAIL_ENTRY signal");
                                        break;
                                    }
                                    VcpuExit::InternalError => {
                                        METRICS.vcpu.failures.inc();
                                        error!("Received KVM_EXIT_INTERNAL_ERROR signal");
                                        break;
                                    }
                                    r => {
                                        METRICS.vcpu.failures.inc();
                                        // TODO: Are we sure we want to finish running a vcpu upon
                                        // receiving a vm exit that is not necessarily an error?
                                        error!("Unexpected exit reason on vcpu run: {:?}", r);
                                        break;
                                    }
                                },
                                Err(vstate::Error::VcpuRun(ref e)) => match e.errno() {
                                    // Why do we check for these if we only return EINVAL?
                                    libc::EAGAIN | libc::EINTR => {}
                                    _ => {
                                        METRICS.vcpu.failures.inc();
                                        error!("Failure during vcpu run: {:?}", e);
                                        break;
                                    }
                                },
                                _ => (),
                            }

                            if kill_signaled.load(Ordering::SeqCst) {
                                break;
                            }
                        }

                        // Nothing we need do for the success case.
                        if let Err(e) = vcpu_exit_evt.write(1) {
                            METRICS.vcpu.failures.inc();
                            error!("Failed signaling vcpu exit event: {:?}", e);
                        }
                    }).map_err(InternalError::VcpuSpawn)?,
            );
        }

        // Starts seccomp filtering before executing guest code.
        // Filters according to specified level.
        // Execution panics if filters cannot be loaded, use --seccomp-level=0 if skipping filters
        // altogether is the desired behaviour.
        match self.seccomp_level {
            seccomp::SECCOMP_LEVEL_ADVANCED => {
                seccomp::setup_seccomp(seccomp::SeccompLevel::Advanced(
                    default_syscalls::default_context().unwrap(),
                )).expect("Could not load filters as requested!");
            }
            seccomp::SECCOMP_LEVEL_BASIC => {
                seccomp::setup_seccomp(seccomp::SeccompLevel::Basic(
                    default_syscalls::ALLOWED_SYSCALLS,
                )).expect("Could not load filters as requested!");
            }
            seccomp::SECCOMP_LEVEL_NONE | _ => {}
        }

        vcpu_thread_barrier.wait();

        Ok(())
    }

    fn load_kernel(&mut self) -> Result<GuestAddress> {
        // This is the easy way out of consuming the value of the kernel_cmdline.
        // TODO: refactor the kernel_cmdline struct in order to have a CString instead of a String.
        // It is safe to unwrap since we've already validated that the kernel_config has a value
        // in the check_health function.
        let kernel_config = self.kernel_config.as_mut().unwrap();
        let cmdline_cstring = CString::new(kernel_config.cmdline.clone())
            .map_err(|_| UserError::KernelCmdLine(kernel_cmdline::Error::InvalidAscii))?;

        // It is safe to unwrap because the VM memory was initialized before in vm.memory_init().
        let vm_memory = self.vm.get_memory().unwrap();
        let entry_addr = kernel_loader::load_kernel(vm_memory, &mut kernel_config.kernel_file)?;
        kernel_loader::load_cmdline(vm_memory, kernel_config.cmdline_addr, &cmdline_cstring)?;

        x86_64::configure_system(
            vm_memory,
            kernel_config.cmdline_addr,
            cmdline_cstring.to_bytes().len() + 1,
            self.vm_config
                .vcpu_count
                .ok_or(InternalError::GeneralFailure)?,
        )?;
        Ok(entry_addr)
    }

    fn register_events(&mut self) -> Result<()> {
        // If the lock is poisoned, it's OK to panic.
        let event_fd = self
            .legacy_device_manager
            .i8042
            .lock()
            .unwrap()
            .get_eventfd_clone()
            .map_err(|_| InternalError::GeneralFailure)?;
        let exit_epoll_evt = self
            .epoll_context
            .add_event(event_fd, EpollDispatch::Exit)?;
        self.exit_evt = Some(exit_epoll_evt);

        self.epoll_context.enable_stdin_event()?;

        Ok(())
    }

    fn start_instance(&mut self) -> Result<()> {
        START_INSTANCE_REQUEST_TS.store(time::precise_time_ns() as usize, Ordering::Release);
        info!("VMM received instance start command");
        if self.is_instance_initialized() {
            return Err(UserError::MicroVMAlreadyRunning)?;
        }

        self.check_health()?;
        // Use unwrap() to crash if the other thread poisoned this lock.
        self.shared_info.write().unwrap().state = InstanceState::Starting;

        self.init_guest_memory()?;

        self.init_devices()?;
        self.init_microvm()?;

        let entry_addr = self.load_kernel()?;

        self.register_events()?;
        self.start_vcpus(entry_addr)?;

        // Use unwrap() to crash if the other thread poisoned this lock.
        self.shared_info.write().unwrap().state = InstanceState::Running;

        // Arm the log write timer.
        // TODO: the timer does not stop on InstanceStop.
        let timer_state = TimerState::Periodic {
            current: Duration::from_secs(WRITE_METRICS_PERIOD_SECONDS),
            interval: Duration::from_secs(WRITE_METRICS_PERIOD_SECONDS),
        };
        self.write_metrics_event
            .fd
            .set_state(timer_state, SetTimeFlags::Default);

        // Log the metrics straight away to check the process startup time.
        if let Err(_) = LOGGER.log_metrics() {
            METRICS.logger.missed_metrics_count.inc();
        }

        Ok(())
    }

    /// Waits for all vCPUs to exit and terminates the Firecracker process.
    fn stop(&mut self, exit_code: i32) {
        info!("Vmm is stopping.");

        if let Some(v) = self.kill_signaled.take() {
            v.store(true, Ordering::SeqCst);
        };

        if let Some(handles) = self.vcpu_handles.take() {
            for handle in handles {
                match handle.kill(VCPU_RTSIG_OFFSET) {
                    Ok(_) => {
                        if let Err(e) = handle.join() {
                            warn!("Failed to join vcpu thread: {:?}", e);
                            METRICS.vcpu.failures.inc();
                        }
                    }
                    Err(e) => {
                        METRICS.vcpu.failures.inc();
                        warn!("Failed to kill vcpu thread: {:?}", e)
                    }
                }
            }
        };

        if let Some(evt) = self.exit_evt.take() {
            if let Err(e) = self.epoll_context.remove_event(evt) {
                warn!(
                    "Cannot remove the exit event from the Epoll Context. {:?}",
                    e
                );
            }
        }

        if let Err(e) = self.epoll_context.disable_stdin_event() {
            warn!("Cannot disable the STDIN event. {:?}", e);
        }

        if let Err(e) = self
            .legacy_device_manager
            .stdin_handle
            .lock()
            .set_canon_mode()
        {
            warn!("Cannot set canonical mode for the terminal. {:?}", e);
        }

        // Log the metrics before exiting.
        if let Err(e) = LOGGER.log_metrics() {
            error!("Failed to log metrics on abort. {}:?", e);
        }

        // Exit from Firecracker using the provided exit code.
        std::process::exit(exit_code);
    }

    fn is_instance_initialized(&self) -> bool {
        let instance_state = {
            // Use unwrap() to crash if the other thread poisoned this lock.
            let shared_info = self.shared_info.read().unwrap();
            shared_info.state.clone()
        };
        match instance_state {
            InstanceState::Uninitialized => false,
            _ => true,
        }
    }

    fn run_control(&mut self) -> Result<()> {
        const EPOLL_EVENTS_LEN: usize = 100;

        let mut events = Vec::<epoll::Event>::with_capacity(EPOLL_EVENTS_LEN);
        // Safe as we pass to set_len the value passed to with_capacity.
        unsafe { events.set_len(EPOLL_EVENTS_LEN) };

        let epoll_raw_fd = self.epoll_context.epoll_raw_fd;

        // TODO: try handling of errors/failures without breaking this main loop.
        'poll: loop {
            let num_events =
                epoll::wait(epoll_raw_fd, -1, &mut events[..]).map_err(InternalError::Poll)?;

            for i in 0..num_events {
                let dispatch_idx = events[i].data() as usize;

                if let Some(dispatch_type) = self.epoll_context.dispatch_table[dispatch_idx] {
                    match dispatch_type {
                        EpollDispatch::Exit => {
                            match self.exit_evt {
                                Some(ref ev) => {
                                    ev.fd.read().map_err(InternalError::EventFd)?;
                                }
                                None => warn!("leftover exit-evt in epollcontext!"),
                            }
                            self.stop(0);
                        }
                        EpollDispatch::Stdin => {
                            let mut out = [0u8; 64];
                            let stdin_lock = self.legacy_device_manager.stdin_handle.lock();
                            match stdin_lock.read_raw(&mut out[..]) {
                                Ok(0) => {
                                    // Zero-length read indicates EOF. Remove from pollables.
                                    self.epoll_context.disable_stdin_event()?;
                                }
                                Err(e) => {
                                    warn!("error while reading stdin: {:?}", e);
                                    self.epoll_context.disable_stdin_event()?;
                                }
                                Ok(count) => {
                                    // Use unwrap() to panic if another thread panicked
                                    // while holding the lock.
                                    self.legacy_device_manager
                                        .stdio_serial
                                        .lock()
                                        .unwrap()
                                        .queue_input_bytes(&out[..count])
                                        .map_err(InternalError::Serial)?;
                                }
                            }
                        }
                        EpollDispatch::DeviceHandler(device_idx, device_token) => {
                            METRICS.vmm.device_events.inc();
                            match self.epoll_context.get_device_handler(device_idx) {
                                Ok(handler) => handler.handle_event(
                                    device_token,
                                    events[i].events().bits(),
                                    EpollHandlerPayload::Empty,
                                ),
                                Err(e) => {
                                    warn!("invalid handler for device {}: {:?}", device_idx, e)
                                }
                            }
                        }
                        EpollDispatch::VmmActionRequest => {
                            self.api_event.fd.read().map_err(InternalError::EventFd)?;
                            self.run_vmm_action().unwrap_or_else(|_| {
                                warn!("got spurious notification from api thread");
                                ()
                            });
                        }
                        EpollDispatch::WriteMetrics => {
                            self.write_metrics_event.fd.read();

                            // Please note that, since LOGGER has no output file configured yet,
                            // it will write to stdout, so metric logging will interfere with
                            // console output.
                            if let Err(e) = LOGGER.log_metrics() {
                                error!("Failed to log metrics: {}", e);
                            }
                        }
                    }
                }
            }
        }
    }

    fn configure_boot_source(
        &mut self,
        kernel_image_path: String,
        kernel_cmdline: Option<String>,
    ) -> std::result::Result<(), BootSourceConfigError> {
        if self.is_instance_initialized() {
            return Err(BootSourceConfigError::UpdateNotAllowedPostBoot);
        }

        let kernel_file =
            File::open(kernel_image_path).map_err(|_| BootSourceConfigError::InvalidKernelPath)?;
        let mut cmdline = kernel_cmdline::Cmdline::new(x86_64::layout::CMDLINE_MAX_SIZE);
        cmdline
            .insert_str(kernel_cmdline.unwrap_or(String::from(DEFAULT_KERNEL_CMDLINE)))
            .map_err(|_| BootSourceConfigError::InvalidKernelCommandLine)?;

        let kernel_config = KernelConfig {
            kernel_file,
            cmdline,
            cmdline_addr: GuestAddress(x86_64::layout::CMDLINE_START),
        };
        self.configure_kernel(kernel_config);

        Ok(())
    }

    fn set_vm_configuration(
        &mut self,
        machine_config: MachineConfiguration,
    ) -> std::result::Result<(), PutMachineConfigurationError> {
        if self.is_instance_initialized() {
            return Err(PutMachineConfigurationError::UpdateNotAllowPostBoot);
        }

        if let Some(vcpu_count_value) = machine_config.vcpu_count {
            // Check that the vcpu_count value is >=1.
            if vcpu_count_value <= 0 {
                return Err(PutMachineConfigurationError::InvalidVcpuCount);
            }
        }

        if let Some(mem_size_mib_value) = machine_config.mem_size_mib {
            // TODO: add other memory checks
            if mem_size_mib_value <= 0 {
                return Err(PutMachineConfigurationError::InvalidMemorySize);
            }
        }

        let ht_enabled = match machine_config.ht_enabled {
            Some(value) => value,
            None => self.vm_config.ht_enabled.unwrap(),
        };

        let vcpu_count_value = match machine_config.vcpu_count {
            Some(value) => value,
            None => self.vm_config.vcpu_count.unwrap(),
        };

        // If hyperthreading is enabled or is to be enabled in this call
        // only allow vcpu count to be 1 or even.
        if ht_enabled && vcpu_count_value > 1 && vcpu_count_value % 2 == 1 {
            return Err(PutMachineConfigurationError::InvalidVcpuCount);
        }

        // Update all the fields that have a new value.
        self.vm_config.vcpu_count = Some(vcpu_count_value);
        self.vm_config.ht_enabled = Some(ht_enabled);

        if machine_config.mem_size_mib.is_some() {
            self.vm_config.mem_size_mib = machine_config.mem_size_mib;
        }

        if machine_config.cpu_template.is_some() {
            self.vm_config.cpu_template = machine_config.cpu_template;
        }

        Ok(())
    }

    fn insert_net_device(
        &mut self,
        body: NetworkInterfaceBody,
    ) -> result::Result<(), NetworkInterfaceError> {
        if self.is_instance_initialized() {
            return Err(NetworkInterfaceError::UpdateNotAllowPostBoot);
        }
        self.network_interface_configs.insert(body)
    }

    fn set_block_device_path(
        &mut self,
        drive_id: String,
        path_on_host: String,
    ) -> result::Result<(), DriveError> {
        let mut block_device = self
            .block_device_configs
            .get_block_device_config(&drive_id)?;
        block_device.path_on_host = PathBuf::from(path_on_host);
        // Try to open the file specified by path_on_host using the permissions of the block_device.
        let disk_file = OpenOptions::new()
            .read(true)
            .write(!block_device.is_read_only())
            .open(block_device.path_on_host())
            .map_err(|_| DriveError::CannotOpenBlockDevice)?;
        self.block_device_configs.update(&block_device)?;
        // When the microvm is running, we also need to update the drive handler and send a
        // rescan command to the drive.
        if self.is_instance_initialized() {
            self.update_drive_handler(&drive_id, disk_file)?;
            self.rescan_block_device(drive_id)?;
        }
        Ok(())
    }

    fn rescan_block_device(&mut self, drive_id: String) -> result::Result<(), DriveError> {
        // Rescan can only happen after the guest is booted.
        if !self.is_instance_initialized() {
            return Err(DriveError::OperationNotAllowedPreBoot);
        }

        // Safe to unwrap() because mmio_device_manager is initialized in init_devices(), which is
        // called before the guest boots, and this function is called after boot.
        let device_manager = self.mmio_device_manager.as_ref().unwrap();
        match device_manager.get_address(&drive_id) {
            Some(&address) => {
                for drive_config in self.block_device_configs.config_list.iter() {
                    if drive_config.drive_id == *drive_id {
                        let metadata = metadata(&drive_config.path_on_host)
                            .map_err(|_| DriveError::BlockDeviceUpdateFailed)?;
                        let new_size = metadata.len();
                        if new_size % virtio::block::SECTOR_SIZE != 0 {
                            warn!(
                                "Disk size {} is not a multiple of sector size {}; \
                                 the remainder will not be visible to the guest.",
                                new_size,
                                virtio::block::SECTOR_SIZE
                            );
                        }
                        return device_manager
                            .update_drive(address, new_size)
                            .map_err(|_| DriveError::BlockDeviceUpdateFailed);
                    }
                }
                Err(DriveError::BlockDeviceUpdateFailed)
            }
            _ => Err(DriveError::InvalidBlockDeviceID),
        }
    }

    // Only call this function as part of the API.
    // If the drive_id does not exist, a new Block Device Config is added to the list.
    fn insert_block_device(
        &mut self,
        block_device_config: BlockDeviceConfig,
    ) -> result::Result<(), DriveError> {
        if self.is_instance_initialized() {
            return Err(DriveError::UpdateNotAllowedPostBoot);
        }
        // If the id of the drive already exists in the list, the operation is update.
        if self
            .block_device_configs
            .contains_drive_id(block_device_config.drive_id.clone())
        {
            self.block_device_configs.update(&block_device_config)
        } else {
            self.block_device_configs
                .add(block_device_config)
                .map(|_| ())
        }
    }

    fn init_logger(
        &self,
        api_logger: APILoggerDescription,
    ) -> std::result::Result<(), APILoggerError> {
        if self.is_instance_initialized() {
            return Err(APILoggerError::InitializationFailure(
                "Cannot initialize logger after boot.".to_string(),
            ));
        }

        let instance_id;
        {
            let guard = self.shared_info.read().unwrap();
            instance_id = guard.id.clone();
        }

        match api_logger.level {
            Some(val) => match val {
                APILoggerLevel::Error => LOGGER.set_level(Level::Error),
                APILoggerLevel::Warning => LOGGER.set_level(Level::Warn),
                APILoggerLevel::Info => LOGGER.set_level(Level::Info),
                APILoggerLevel::Debug => LOGGER.set_level(Level::Debug),
            },
            None => (),
        }

        if let Some(val) = api_logger.show_log_origin {
            LOGGER.set_include_origin(val, val);
        }

        if let Some(val) = api_logger.show_level {
            LOGGER.set_include_level(val);
        }

        LOGGER
            .init(
                &instance_id,
                Some(api_logger.log_fifo),
                Some(api_logger.metrics_fifo),
            ).map_err(|e| APILoggerError::InitializationFailure(e.to_string()))?;

        Ok(())
    }

    fn send_response(response: Box<GenerateResponse + Send>, sender: OutcomeSender) {
        sender
            .send(response)
            .map_err(|_| ())
            .expect("one-shot channel closed");
    }

    fn run_vmm_action(&mut self) -> Result<()> {
        let request = match self.from_api.try_recv() {
            Ok(t) => *t,
            Err(TryRecvError::Empty) => {
                return Err(InternalError::ApiChannel)?;
            }
            Err(TryRecvError::Disconnected) => {
                panic!();
            }
        };

        match request {
            VmmAction::ConfigureBootSource(boot_source_body, sender) => {
                let boxed_response = match boot_source_body.local_image {
                    // Check that the kernel path exists and it is valid.
                    Some(local_image) => Box::new(self.configure_boot_source(
                        local_image.kernel_image_path,
                        boot_source_body.boot_args,
                    )),
                    None => Box::new(Err(BootSourceConfigError::EmptyKernelPath)),
                };
                Vmm::send_response(boxed_response, sender);
            }
            VmmAction::ConfigureLogger(logger_description, sender) => {
                Vmm::send_response(Box::new(self.init_logger(logger_description)), sender);
            }
            VmmAction::GetMachineConfiguration(sender) => {
                Vmm::send_response(Box::new(self.vm_config.clone()), sender);
            }
            VmmAction::InsertBlockDevice(block_device_config, sender) => {
                Vmm::send_response(
                    Box::new(self.insert_block_device(block_device_config)),
                    sender,
                );
            }
            VmmAction::InsertNetworkDevice(netif_body, sender) => {
                Vmm::send_response(Box::new(self.insert_net_device(netif_body)), sender);
            }
            VmmAction::RescanBlockDevice(drive_id, sender) => {
                Vmm::send_response(Box::new(self.rescan_block_device(drive_id)), sender);
            }
            VmmAction::StartMicroVm(sender) => {
                let boxed_response = match self.start_instance() {
                    Ok(_) => Box::new(Ok(())),
                    Err(e) => {
                        let err_type = match e {
                            Error::User(_) => ErrorType::UserError,
                            Error::Internal(_) => ErrorType::InternalError,
                        };

                        Box::new(Err(SyncError::InstanceStartFailed(
                            err_type,
                            format!("Failed to start microVM. {:?}", e),
                        )))
                    }
                };
                Vmm::send_response(boxed_response, sender);
            }
            VmmAction::SetVmConfiguration(machine_config_body, sender) => {
                Vmm::send_response(
                    Box::new(self.set_vm_configuration(machine_config_body)),
                    sender,
                );
            }
            VmmAction::UpdateDrivePath(drive_id, path_on_host, sender) => {
                Vmm::send_response(
                    Box::new(self.set_block_device_path(drive_id, path_on_host)),
                    sender,
                );
            }
        };

        Ok(())
    }
}

/// Starts a new vmm thread that can service API requests.
///
/// # Arguments
///
/// * `api_shared_info` - A parameter for storing information on the VMM (e.g the current state).
/// * `api_event_fd` - An event fd used for receiving API associated events.
/// * `from_api` - The receiver end point of the communication channel.
/// * `seccomp_level` - The level of seccomp filtering used. Filters are loaded before executing
///                     guest code.
///                     See `seccomp::SeccompLevel` for more information about seccomp levels.
pub fn start_vmm_thread(
    api_shared_info: Arc<RwLock<InstanceInfo>>,
    api_event_fd: EventFd,
    from_api: Receiver<Box<VmmAction>>,
    seccomp_level: u32,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("fc_vmm".to_string())
        .spawn(move || {
            // If this fails, consider it fatal. Use expect().
            let mut vmm = Vmm::new(api_shared_info, api_event_fd, from_api, seccomp_level)
                .expect("Cannot create VMM.");
            match vmm.run_control() {
                Ok(()) => vmm.stop(0),
                Err(_) => vmm.stop(1),
            }
        }).expect("VMM thread spawn failed.")
}

#[cfg(test)]
mod tests {
    extern crate tempfile;

    use super::*;

    use std::fs::File;
    use std::io::{BufRead, BufReader};
    use std::sync::atomic::AtomicUsize;

    use self::tempfile::NamedTempFile;
    use data_model::vm::{CpuFeaturesTemplate, DeviceState};
    use devices::virtio::ActivateResult;
    use net_util::MacAddr;

    impl Vmm {
        fn get_kernel_cmdline_str(&self) -> &str {
            if let Some(ref k) = self.kernel_config {
                k.cmdline.as_str()
            } else {
                ""
            }
        }

        fn remove_addr(&mut self, id: &String) {
            self.mmio_device_manager
                .as_mut()
                .unwrap()
                .remove_address(id);
        }

        fn default_kernel_config(&mut self) {
            let kernel_file_temp =
                NamedTempFile::new().expect("Failed to create temporary kernel file.");
            let kernel_path = String::from(kernel_file_temp.path().to_path_buf().to_str().unwrap());
            let kernel_file = File::open(kernel_path).unwrap();

            let mut cmdline = kernel_cmdline::Cmdline::new(x86_64::layout::CMDLINE_MAX_SIZE);
            assert!(cmdline.insert_str(DEFAULT_KERNEL_CMDLINE).is_ok());
            let kernel_cfg = KernelConfig {
                cmdline,
                kernel_file,
                cmdline_addr: GuestAddress(x86_64::layout::CMDLINE_START),
            };
            self.configure_kernel(kernel_cfg);
        }

        fn set_instance_state(&mut self, instance_state: InstanceState) {
            self.shared_info.write().unwrap().state = instance_state;
        }
    }

    struct DummyEpollHandler {
        pub evt: Option<DeviceEventT>,
        pub flags: Option<u32>,
        pub payload: Option<EpollHandlerPayload>,
    }

    impl EpollHandler for DummyEpollHandler {
        fn handle_event(
            &mut self,
            device_event: DeviceEventT,
            event_flags: u32,
            payload: EpollHandlerPayload,
        ) {
            self.evt = Some(device_event);
            self.flags = Some(event_flags);
            self.payload = Some(payload);
        }
    }

    #[allow(dead_code)]
    #[derive(Clone)]
    struct DummyDevice {
        dummy: u32,
    }

    impl devices::virtio::VirtioDevice for DummyDevice {
        fn device_type(&self) -> u32 {
            0
        }

        fn queue_max_sizes(&self) -> &[u16] {
            &[10]
        }

        #[allow(unused_variables)]
        #[allow(unused_mut)]
        fn activate(
            &mut self,
            mem: GuestMemory,
            interrupt_evt: EventFd,
            status: Arc<AtomicUsize>,
            queues: Vec<devices::virtio::Queue>,
            mut queue_evts: Vec<EventFd>,
        ) -> ActivateResult {
            Ok(())
        }
    }

    fn create_vmm_object(state: InstanceState) -> Vmm {
        let shared_info = Arc::new(RwLock::new(InstanceInfo {
            state,
            id: "TEST_ID".to_string(),
        }));

        let (_to_vmm, from_api) = channel();
        let vmm = Vmm::new(
            shared_info,
            EventFd::new().expect("cannot create eventFD"),
            from_api,
            seccomp::SECCOMP_LEVEL_ADVANCED,
        ).expect("Cannot Create VMM");
        return vmm;
    }

    #[test]
    fn test_device_handler() {
        let mut ep = EpollContext::new().unwrap();
        let (base, sender) = ep.allocate_tokens(1);
        assert_eq!(ep.device_handlers.len(), 1);
        assert_eq!(base, 1);

        let handler = DummyEpollHandler {
            evt: None,
            flags: None,
            payload: None,
        };
        assert!(sender.send(Box::new(handler)).is_ok());
        assert!(ep.get_device_handler(0).is_ok());
    }

    #[test]
    fn test_put_block_device() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        let f = NamedTempFile::new().unwrap();
        // Test that creating a new block device returns the correct output (i.e. "Created").
        let root_block_device = BlockDeviceConfig {
            drive_id: String::from("root"),
            path_on_host: f.path().to_path_buf(),
            is_root_device: true,
            partuuid: None,
            is_read_only: false,
            rate_limiter: None,
        };
        assert!(vmm.insert_block_device(root_block_device.clone()).is_ok());
        assert!(
            vmm.block_device_configs
                .config_list
                .contains(&root_block_device)
        );

        // Test that updating a block device returns the correct output (i.e. "Updated").
        let root_block_device = BlockDeviceConfig {
            drive_id: String::from("root"),
            path_on_host: f.path().to_path_buf(),
            is_root_device: true,
            partuuid: None,
            is_read_only: true,
            rate_limiter: None,
        };
        assert!(vmm.insert_block_device(root_block_device.clone()).is_ok());
        assert!(
            vmm.block_device_configs
                .config_list
                .contains(&root_block_device)
        );
    }

    #[test]
    fn test_put_net_device() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);

        // test create network interface
        let network_interface = NetworkInterfaceBody {
            iface_id: String::from("netif"),
            state: DeviceState::Attached,
            host_dev_name: String::from("hostname"),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
            allow_mmds_requests: false,
        };
        assert!(vmm.insert_net_device(network_interface).is_ok());

        if let Ok(mac) = MacAddr::parse_str("01:23:45:67:89:0A") {
            // test update network interface
            let network_interface = NetworkInterfaceBody {
                iface_id: String::from("netif"),
                state: DeviceState::Attached,
                host_dev_name: String::from("hostname2"),
                guest_mac: Some(mac),
                rx_rate_limiter: None,
                tx_rate_limiter: None,
                allow_mmds_requests: false,
            };
            assert!(vmm.insert_net_device(network_interface).is_ok());
        }
    }

    #[test]
    fn test_machine_configuration() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);

        // test the default values of machine config
        // vcpu_count = 1
        assert_eq!(vmm.vm_config.vcpu_count, Some(1));
        // mem_size = 128
        assert_eq!(vmm.vm_config.mem_size_mib, Some(128));
        // ht_enabled = false
        assert_eq!(vmm.vm_config.ht_enabled, Some(false));
        // no cpu template
        assert!(vmm.vm_config.cpu_template.is_none());

        // 1. Tests with no hyperthreading
        // test put machine configuration for vcpu count with valid value
        let machine_config = MachineConfiguration {
            vcpu_count: Some(3),
            mem_size_mib: None,
            ht_enabled: None,
            cpu_template: None,
        };
        assert!(vmm.set_vm_configuration(machine_config).is_ok());
        assert_eq!(vmm.vm_config.vcpu_count, Some(3));
        assert_eq!(vmm.vm_config.mem_size_mib, Some(128));
        assert_eq!(vmm.vm_config.ht_enabled, Some(false));

        // test put machine configuration for mem size with valid value
        let machine_config = MachineConfiguration {
            vcpu_count: None,
            mem_size_mib: Some(256),
            ht_enabled: None,
            cpu_template: None,
        };
        assert!(vmm.set_vm_configuration(machine_config).is_ok());
        assert_eq!(vmm.vm_config.vcpu_count, Some(3));
        assert_eq!(vmm.vm_config.mem_size_mib, Some(256));
        assert_eq!(vmm.vm_config.ht_enabled, Some(false));

        // Test Error cases for put_machine_configuration with invalid value for vcpu_count
        // Test that the put method return error & that the vcpu value is not changed
        let machine_config = MachineConfiguration {
            vcpu_count: Some(0),
            mem_size_mib: None,
            ht_enabled: None,
            cpu_template: None,
        };
        assert_eq!(
            vmm.set_vm_configuration(machine_config).unwrap_err(),
            PutMachineConfigurationError::InvalidVcpuCount
        );
        assert_eq!(vmm.vm_config.vcpu_count, Some(3));

        // Test Error cases for put_machine_configuration with invalid value for the mem_size_mib
        // Test that the put method return error & that the mem_size_mib value is not changed
        let machine_config = MachineConfiguration {
            vcpu_count: Some(1),
            mem_size_mib: Some(0),
            ht_enabled: Some(false),
            cpu_template: Some(CpuFeaturesTemplate::T2),
        };
        assert_eq!(
            vmm.set_vm_configuration(machine_config).unwrap_err(),
            PutMachineConfigurationError::InvalidMemorySize
        );
        assert_eq!(vmm.vm_config.vcpu_count, Some(3));
        assert_eq!(vmm.vm_config.mem_size_mib, Some(256));
        assert_eq!(vmm.vm_config.ht_enabled, Some(false));
        assert!(vmm.vm_config.cpu_template.is_none());

        // 2. Test with hyperthreading enabled
        // Test that you can't change the hyperthreading value to false when the vcpu count
        // is odd
        let machine_config = MachineConfiguration {
            vcpu_count: None,
            mem_size_mib: None,
            ht_enabled: Some(true),
            cpu_template: None,
        };
        assert_eq!(
            vmm.set_vm_configuration(machine_config).unwrap_err(),
            PutMachineConfigurationError::InvalidVcpuCount
        );
        assert_eq!(vmm.vm_config.ht_enabled, Some(false));
        // Test that you can change the ht flag when you have a valid vcpu count
        // Also set the CPU Template since we are here
        let machine_config = MachineConfiguration {
            vcpu_count: Some(2),
            mem_size_mib: None,
            ht_enabled: Some(true),
            cpu_template: Some(CpuFeaturesTemplate::T2),
        };
        assert!(vmm.set_vm_configuration(machine_config).is_ok());
        assert_eq!(vmm.vm_config.vcpu_count, Some(2));
        assert_eq!(vmm.vm_config.ht_enabled, Some(true));
        assert_eq!(vmm.vm_config.cpu_template, Some(CpuFeaturesTemplate::T2));
    }

    #[test]
    fn new_epoll_context_test() {
        assert!(EpollContext::new().is_ok());
    }

    #[test]
    fn enable_disable_stdin_test() {
        let mut ep = EpollContext::new().unwrap();
        // enabling stdin should work
        assert!(ep.enable_stdin_event().is_ok());

        // doing it again should fail
        // TODO: commented out because stdin & /dev/null related issues, as mentioned in another
        // comment from enable_stdin_event().
        // assert!(ep.enable_stdin_event().is_err());

        // disabling stdin should work
        assert!(ep.disable_stdin_event().is_ok());

        // enabling stdin should work now
        assert!(ep.enable_stdin_event().is_ok());
        // disabling it again should work
        assert!(ep.disable_stdin_event().is_ok());
    }

    #[test]
    fn add_remove_event_test() {
        let mut ep = EpollContext::new().unwrap();
        let evfd = EventFd::new().unwrap();

        // adding new event should work
        let epev = ep.add_event(evfd, EpollDispatch::Exit);
        assert!(epev.is_ok());

        // removing event should work
        assert!(ep.remove_event(epev.unwrap()).is_ok());
    }

    #[test]
    fn epoll_event_test() {
        let mut ep = EpollContext::new().unwrap();
        let evfd = EventFd::new().unwrap();

        // adding new event should work
        let epev = ep.add_event(evfd, EpollDispatch::Exit);
        assert!(epev.is_ok());
        let epev = epev.unwrap();

        let evpoll_events_len = 10;
        let mut events = Vec::<epoll::Event>::with_capacity(evpoll_events_len);
        // Safe as we pass to set_len the value passed to with_capacity.
        unsafe { events.set_len(evpoll_events_len) };

        // epoll should have no pending events
        let epollret = epoll::wait(ep.epoll_raw_fd, 0, &mut events[..]);
        let num_events = epollret.unwrap();
        assert_eq!(num_events, 0);

        // raise the event
        assert!(epev.fd.write(1).is_ok());

        // epoll should report one event
        let epollret = epoll::wait(ep.epoll_raw_fd, 0, &mut events[..]);
        let num_events = epollret.unwrap();
        assert_eq!(num_events, 1);

        // reported event should be the one we raised
        let idx = events[0].data() as usize;
        assert!(ep.dispatch_table[idx].is_some());
        assert_eq!(
            *ep.dispatch_table[idx].as_ref().unwrap(),
            EpollDispatch::Exit
        );

        // removing event should work
        assert!(ep.remove_event(epev).is_ok());
    }

    #[test]
    fn epoll_event_try_get_after_remove_test() {
        let mut ep = EpollContext::new().unwrap();
        let evfd = EventFd::new().unwrap();

        // adding new event should work
        let epev = ep.add_event(evfd, EpollDispatch::Exit).unwrap();

        let evpoll_events_len = 10;
        let mut events = Vec::<epoll::Event>::with_capacity(evpoll_events_len);
        // Safe as we pass to set_len the value passed to with_capacity.
        unsafe { events.set_len(evpoll_events_len) };

        // raise the event
        assert!(epev.fd.write(1).is_ok());

        // removing event should work
        assert!(ep.remove_event(epev).is_ok());

        // epoll should have no pending events
        let epollret = epoll::wait(ep.epoll_raw_fd, 0, &mut events[..]);
        let num_events = epollret.unwrap();
        assert_eq!(num_events, 0);
    }

    #[test]
    fn epoll_event_try_use_after_remove_test() {
        let mut ep = EpollContext::new().unwrap();
        let evfd = EventFd::new().unwrap();

        // adding new event should work
        let epev = ep.add_event(evfd, EpollDispatch::Exit).unwrap();

        let evpoll_events_len = 10;
        let mut events = Vec::<epoll::Event>::with_capacity(evpoll_events_len);
        // Safe as we pass to set_len the value passed to with_capacity.
        unsafe { events.set_len(evpoll_events_len) };

        // raise the event
        assert!(epev.fd.write(1).is_ok());

        // epoll should report one event
        let epollret = epoll::wait(ep.epoll_raw_fd, 0, &mut events[..]);
        let num_events = epollret.unwrap();
        assert_eq!(num_events, 1);

        // removing event should work
        assert!(ep.remove_event(epev).is_ok());

        // reported event should no longer be available
        let idx = events[0].data() as usize;
        assert!(ep.dispatch_table[idx].is_none());
    }

    #[test]
    fn test_kvm_context() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::io::FromRawFd;

        let c = KvmContext::new().unwrap();
        let nr_vcpus = c.nr_vcpus();
        let max_vcpus = c.max_vcpus();

        assert!(nr_vcpus > 0);
        assert!(max_vcpus >= nr_vcpus);

        let kvm = Kvm::new().unwrap();
        let f = unsafe { File::from_raw_fd(kvm.as_raw_fd()) };
        let m1 = f.metadata().unwrap();
        let m2 = File::open("/dev/kvm").unwrap().metadata().unwrap();

        assert_eq!(m1.dev(), m2.dev());
        assert_eq!(m1.ino(), m2.ino());
    }

    #[test]
    fn test_check_health() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        assert!(vmm.check_health().is_err());

        let dummy_addr = GuestAddress(0x1000);
        vmm.configure_kernel(KernelConfig {
            cmdline_addr: dummy_addr,
            cmdline: kernel_cmdline::Cmdline::new(10),
            kernel_file: tempfile::tempfile().unwrap(),
        });
        assert!(vmm.check_health().is_ok());
    }

    #[test]
    fn test_is_instance_initialized() {
        let vmm = create_vmm_object(InstanceState::Uninitialized);
        assert_eq!(vmm.is_instance_initialized(), false);

        let vmm = create_vmm_object(InstanceState::Starting);
        assert_eq!(vmm.is_instance_initialized(), true);

        let vmm = create_vmm_object(InstanceState::Halting);
        assert_eq!(vmm.is_instance_initialized(), true);

        let vmm = create_vmm_object(InstanceState::Halted);
        assert_eq!(vmm.is_instance_initialized(), true);

        let vmm = create_vmm_object(InstanceState::Running);
        assert_eq!(vmm.is_instance_initialized(), true);
    }

    #[test]
    fn test_attach_block_devices() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        let block_file = NamedTempFile::new().unwrap();

        // Use Case 1: Root Block Device is not specified through PARTUUID.
        let root_block_device = BlockDeviceConfig {
            drive_id: String::from("root"),
            path_on_host: block_file.path().to_path_buf(),
            is_root_device: true,
            partuuid: None,
            is_read_only: false,
            rate_limiter: None,
        };
        // Test that creating a new block device returns the correct output.
        assert!(vmm.insert_block_device(root_block_device.clone()).is_ok());
        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.guest_memory.is_some());

        vmm.default_kernel_config();

        let guest_mem = vmm.guest_memory.clone().unwrap();
        let mut device_manager =
            MMIODeviceManager::new(guest_mem.clone(), x86_64::get_32bit_gap_start() as u64);
        assert!(vmm.attach_block_devices(&mut device_manager).is_ok());
        assert!(vmm.get_kernel_cmdline_str().contains("root=/dev/vda"));

        // Use Case 2: Root Block Device is specified through PARTUUID.
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        let root_block_device = BlockDeviceConfig {
            drive_id: String::from("root"),
            path_on_host: block_file.path().to_path_buf(),
            is_root_device: true,
            partuuid: Some("0eaa91a0-01".to_string()),
            is_read_only: false,
            rate_limiter: None,
        };

        // Test that creating a new block device returns the correct output.
        assert!(vmm.insert_block_device(root_block_device.clone()).is_ok());
        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.guest_memory.is_some());

        vmm.default_kernel_config();

        let guest_mem = vmm.guest_memory.clone().unwrap();
        let mut device_manager =
            MMIODeviceManager::new(guest_mem.clone(), x86_64::get_32bit_gap_start() as u64);
        assert!(vmm.attach_block_devices(&mut device_manager).is_ok());
        assert!(
            vmm.get_kernel_cmdline_str()
                .contains("root=PARTUUID=0eaa91a0-01")
        );

        // Use Case 3: Root Block Device is not added at all.
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        let non_root_block_device = BlockDeviceConfig {
            drive_id: String::from("not_root"),
            path_on_host: block_file.path().to_path_buf(),
            is_root_device: false,
            partuuid: Some("0eaa91a0-01".to_string()),
            is_read_only: false,
            rate_limiter: None,
        };

        // Test that creating a new block device returns the correct output.
        assert!(
            vmm.insert_block_device(non_root_block_device.clone())
                .is_ok()
        );
        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.guest_memory.is_some());

        vmm.default_kernel_config();

        let guest_mem = vmm.guest_memory.clone().unwrap();
        let mut device_manager =
            MMIODeviceManager::new(guest_mem.clone(), x86_64::get_32bit_gap_start() as u64);
        assert!(vmm.attach_block_devices(&mut device_manager).is_ok());
        // Test that kernel commandline does not contain either /dev/vda or PARTUUID.
        assert!(!vmm.get_kernel_cmdline_str().contains("root=PARTUUID="));
        assert!(!vmm.get_kernel_cmdline_str().contains("root=/dev/vda"));

        // Test that the non root device is attached.
        assert!(
            device_manager
                .get_address(&non_root_block_device.drive_id)
                .is_some()
        );

        // Test partial update of block devices.
        let new_block = NamedTempFile::new().unwrap();
        let path = String::from(new_block.path().to_path_buf().to_str().unwrap());
        assert!(
            vmm.set_block_device_path("not_root".to_string(), path)
                .is_ok()
        );
    }

    #[test]
    fn test_attach_net_devices() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.guest_memory.is_some());

        vmm.default_kernel_config();

        let guest_mem = vmm.guest_memory.clone().unwrap();
        let mut device_manager =
            MMIODeviceManager::new(guest_mem.clone(), x86_64::get_32bit_gap_start() as u64);

        // test create network interface
        let network_interface = NetworkInterfaceBody {
            iface_id: String::from("netif"),
            state: DeviceState::Attached,
            host_dev_name: String::from("hostname3"),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
            allow_mmds_requests: false,
        };

        assert!(vmm.insert_net_device(network_interface).is_ok());

        assert!(vmm.attach_net_devices(&mut device_manager).is_ok());
        // a second call to attach_net_devices should fail because when
        // we are creating the virtio::Net object, we are taking the tap.
        assert!(vmm.attach_net_devices(&mut device_manager).is_err());
    }

    #[test]
    fn test_init_devices() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        vmm.default_kernel_config();
        assert!(vmm.init_guest_memory().is_ok());

        assert!(vmm.init_devices().is_ok());
    }

    #[test]
    fn test_rescan() {
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        vmm.default_kernel_config();

        let root_file = NamedTempFile::new().unwrap();
        let scratch_file = NamedTempFile::new().unwrap();

        let root_block_device = BlockDeviceConfig {
            drive_id: String::from("root"),
            path_on_host: root_file.path().to_path_buf(),
            is_root_device: true,
            partuuid: None,
            is_read_only: false,
            rate_limiter: None,
        };
        let non_root_block_device = BlockDeviceConfig {
            drive_id: String::from("not_root"),
            path_on_host: scratch_file.path().to_path_buf(),
            is_root_device: false,
            partuuid: None,
            is_read_only: true,
            rate_limiter: None,
        };

        assert!(vmm.insert_block_device(root_block_device.clone()).is_ok());
        assert!(
            vmm.insert_block_device(non_root_block_device.clone())
                .is_ok()
        );

        assert!(vmm.init_guest_memory().is_ok());
        assert!(vmm.guest_memory.is_some());

        let guest_mem = vmm.guest_memory.clone().unwrap();
        let mut device_manager =
            MMIODeviceManager::new(guest_mem.clone(), x86_64::get_32bit_gap_start() as u64);

        let dummy_box = Box::new(DummyDevice { dummy: 0 });
        // use a dummy command line as it is not used in this test.
        let _addr = device_manager
            .register_device(
                dummy_box,
                &mut kernel_cmdline::Cmdline::new(x86_64::layout::CMDLINE_MAX_SIZE),
                Some(String::from("not_root")),
            ).unwrap();

        vmm.mmio_device_manager = Some(device_manager);
        vmm.set_instance_state(InstanceState::Running);

        // Test valid rescan_block_device.
        assert!(vmm.rescan_block_device("not_root".to_string()).is_ok());

        // Test rescan_block_device with invalid ID.
        assert!(vmm.rescan_block_device("foo".to_string()).is_err());

        // Test rescan_block_device with invalid device address.
        vmm.remove_addr(&String::from("not_root"));
        assert!(vmm.rescan_block_device("not_root".to_string()).is_err());

        // Test rescan not allowed.
        let mut vmm = create_vmm_object(InstanceState::Uninitialized);
        assert!(
            vmm.insert_block_device(non_root_block_device.clone())
                .is_ok()
        );
        assert_eq!(
            vmm.rescan_block_device("not_root".to_string()).unwrap_err(),
            DriveError::OperationNotAllowedPreBoot
        );
    }

    // Helper function that tests whether the log file contains the `line_tokens`
    fn validate_logs(
        log_path: &str,
        line_tokens: &[(&'static str, &'static str, &'static str)],
    ) -> bool {
        let f = File::open(log_path).unwrap();
        let mut reader = BufReader::new(f);
        let mut res = true;
        let mut line = String::new();
        for tuple in line_tokens {
            line.clear();
            reader.read_line(&mut line).unwrap();
            res &= line.contains(&tuple.0);
            res &= line.contains(&tuple.1);
            res &= line.contains(&tuple.2);
        }
        res
    }

    #[test]
    fn test_init_logger_from_api() {
        // Error case: update after instance is running
        let log_file = NamedTempFile::new().unwrap();
        let metrics_file = NamedTempFile::new().unwrap();
        let desc = APILoggerDescription {
            log_fifo: log_file.path().to_str().unwrap().to_string(),
            metrics_fifo: metrics_file.path().to_str().unwrap().to_string(),
            level: Some(APILoggerLevel::Warning),
            show_level: Some(true),
            show_log_origin: Some(true),
        };

        let mut vmm = create_vmm_object(InstanceState::Running);
        assert!(vmm.init_logger(desc).is_err());

        // Reset vmm state to test the other scenarios.
        vmm.set_instance_state(InstanceState::Uninitialized);

        // Error case: initializing logger with invalid pipes return error.
        let desc = APILoggerDescription {
            log_fifo: String::from("not_found_file_log"),
            metrics_fifo: String::from("not_found_file_metrics"),
            level: None,
            show_level: None,
            show_log_origin: None,
        };
        assert!(vmm.init_logger(desc).is_err());

        // Initializing logger with valid pipes is ok.
        let log_file = NamedTempFile::new().unwrap();
        let metrics_file = NamedTempFile::new().unwrap();
        let desc = APILoggerDescription {
            log_fifo: log_file.path().to_str().unwrap().to_string(),
            metrics_fifo: metrics_file.path().to_str().unwrap().to_string(),
            level: Some(APILoggerLevel::Warning),
            show_level: Some(true),
            show_log_origin: Some(true),
        };
        assert!(vmm.init_logger(desc).is_ok());

        info!("info");
        warn!("warning");
        error!("error");

        // Assert that the log contains the error and warning.
        assert!(validate_logs(
            log_file.path().to_str().unwrap(),
            &[
                ("WARN", "vmm/src/lib.rs", "warn"),
                ("ERROR", "vmm/src/lib.rs", "error"),
            ]
        ));
    }
}
