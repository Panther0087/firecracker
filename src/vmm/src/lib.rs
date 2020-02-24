// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! Virtual Machine Monitor that leverages the Linux Kernel-based Virtual Machine (KVM),
//! and other virtualization features to run a single lightweight micro-virtual
//! machine (microVM).
#![deny(missing_docs)]

extern crate kvm_bindings;
extern crate kvm_ioctls;
extern crate libc;
extern crate polly;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

extern crate arch;
#[cfg(target_arch = "x86_64")]
extern crate cpuid;
extern crate devices;
extern crate kernel;
#[macro_use]
extern crate logger;
extern crate dumbo;
extern crate rate_limiter;
extern crate seccomp;
extern crate utils;
extern crate vm_memory;

/// Handles setup and initialization a `Vmm` object.
pub mod builder;
/// Handles runtime configuration of a `Vmm` object.
pub mod controller;
/// Syscalls allowed through the seccomp filter.
pub mod default_syscalls;
pub(crate) mod device_manager;
/// Resource store for configured microVM resources.
pub mod resources;
/// microVM RPC API adapters.
pub mod rpc_interface;
/// Signal handling utilities.
pub mod signal_handler;
/// Wrappers over structures used to configure the VMM.
pub mod vmm_config;
mod vstate;

#[cfg(target_arch = "aarch64")]
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::Mutex;
use std::time::Duration;

use arch::DeviceType;
use arch::InitrdConfig;
#[cfg(target_arch = "x86_64")]
use device_manager::legacy::PortIODeviceManager;
#[cfg(target_arch = "aarch64")]
use device_manager::mmio::MMIODeviceInfo;
use device_manager::mmio::MMIODeviceManager;
use devices::BusDevice;
use kernel::cmdline::Cmdline as KernelCmdline;
use logger::error::LoggerError;
use logger::LOGGER;
use polly::epoll::{EpollEvent, EventSet};
use polly::event_manager::{self, EventManager, Subscriber};
use seccomp::{BpfProgram, BpfProgramRef, SeccompFilter};
use utils::eventfd::EventFd;
use utils::time::TimestampUs;
use vm_memory::GuestMemoryMmap;
use vstate::{Vcpu, VcpuEvent, VcpuHandle, VcpuResponse, Vm};

/// Success exit code.
pub const FC_EXIT_CODE_OK: u8 = 0;
/// Generic error exit code.
pub const FC_EXIT_CODE_GENERIC_ERROR: u8 = 1;
/// Generic exit code for an error considered not possible to occur if the program logic is sound.
pub const FC_EXIT_CODE_UNEXPECTED_ERROR: u8 = 2;
/// Firecracker was shut down after intercepting a restricted system call.
pub const FC_EXIT_CODE_BAD_SYSCALL: u8 = 148;
/// Firecracker was shut down after intercepting `SIGBUS`.
pub const FC_EXIT_CODE_SIGBUS: u8 = 149;
/// Firecracker was shut down after intercepting `SIGSEGV`.
pub const FC_EXIT_CODE_SIGSEGV: u8 = 150;
/// Bad configuration for microvm's resources, when using a single json.
pub const FC_EXIT_CODE_BAD_CONFIGURATION: u8 = 151;
/// Command line arguments parsing error.
pub const FC_EXIT_CODE_ARG_PARSING: u8 = 152;

/// Errors associated with the VMM internal logic. These errors cannot be generated by direct user
/// input, but can result from bad configuration of the host (for example if Firecracker doesn't
/// have permissions to open the KVM fd).
#[derive(Debug)]
pub enum Error {
    /// This error is thrown by the minimal boot loader implementation.
    ConfigureSystem(arch::Error),
    /// Legacy devices work with Event file descriptors and the creation can fail because
    /// of resource exhaustion.
    #[cfg(target_arch = "x86_64")]
    CreateLegacyDevice(device_manager::legacy::Error),
    /// Cannot read from an Event file descriptor.
    EventFd(io::Error),
    /// Polly error wrapper.
    EventManager(event_manager::Error),
    /// I8042 Error.
    I8042Error(devices::legacy::I8042DeviceError),
    /// Cannot access kernel file.
    KernelFile(io::Error),
    /// Cannot open /dev/kvm. Either the host does not have KVM or Firecracker does not have
    /// permission to open the file descriptor.
    KvmContext(vstate::Error),
    #[cfg(target_arch = "x86_64")]
    /// Cannot add devices to the Legacy I/O Bus.
    LegacyIOBus(device_manager::legacy::Error),
    /// Cannot load command line.
    LoadCommandline(kernel::cmdline::Error),
    /// Internal logger error.
    Logger(LoggerError),
    /// Cannot add a device to the MMIO Bus.
    RegisterMMIODevice(device_manager::mmio::Error),
    /// Cannot build seccomp filters.
    SeccompFilters(seccomp::Error),
    /// Write to the serial console failed.
    Serial(io::Error),
    /// Cannot create Timer file descriptor.
    TimerFd(io::Error),
    /// Vcpu error.
    Vcpu(vstate::Error),
    /// Cannot send event to vCPU.
    VcpuEvent(vstate::Error),
    /// Cannot create a vCPU handle.
    VcpuHandle(vstate::Error),
    /// vCPU resume failed.
    VcpuResume,
    /// Cannot spawn a new Vcpu thread.
    VcpuSpawn(std::io::Error),
    /// Vm error.
    Vm(vstate::Error),
    /// Error thrown by observer object on Vmm initialization.
    VmmObserverInit(utils::errno::Error),
    /// Error thrown by observer object on Vmm teardown.
    VmmObserverTeardown(utils::errno::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            ConfigureSystem(e) => write!(f, "System configuration error: {:?}", e),
            #[cfg(target_arch = "x86_64")]
            CreateLegacyDevice(e) => write!(f, "Error creating legacy device: {:?}", e),
            EventFd(e) => write!(f, "Event fd error: {}", e),
            EventManager(e) => write!(f, "Event manager error: {:?}", e),
            I8042Error(e) => write!(f, "I8042 error: {}", e),
            KernelFile(e) => write!(f, "Cannot access kernel file: {}", e),
            KvmContext(e) => write!(f, "Failed to validate KVM support: {:?}", e),
            #[cfg(target_arch = "x86_64")]
            LegacyIOBus(e) => write!(f, "Cannot add devices to the legacy I/O Bus. {}", e),
            LoadCommandline(e) => write!(f, "Cannot load command line: {}", e),
            Logger(e) => write!(f, "Logger error: {}", e),
            RegisterMMIODevice(e) => write!(f, "Cannot add a device to the MMIO Bus. {}", e),
            SeccompFilters(e) => write!(f, "Cannot build seccomp filters: {}", e),
            Serial(e) => write!(f, "Error writing to the serial console: {:?}", e),
            TimerFd(e) => write!(f, "Error creating timer fd: {}", e),
            Vcpu(e) => write!(f, "Vcpu error: {}", e),
            VcpuEvent(e) => write!(f, "Cannot send event to vCPU. {:?}", e),
            VcpuHandle(e) => write!(f, "Cannot create a vCPU handle. {}", e),
            VcpuResume => write!(f, "vCPUs resume failed."),
            VcpuSpawn(e) => write!(f, "Cannot spawn Vcpu thread: {}", e),
            Vm(e) => write!(f, "Vm error: {}", e),
            VmmObserverInit(e) => write!(
                f,
                "Error thrown by observer object on Vmm initialization: {}",
                e
            ),
            VmmObserverTeardown(e) => {
                write!(f, "Error thrown by observer object on Vmm teardown: {}", e)
            }
        }
    }
}

/// Trait for objects that need custom initialization and teardown during the Vmm lifetime.
pub trait VmmEventsObserver {
    /// This function will be called during microVm boot.
    fn on_vmm_boot(&mut self) -> std::result::Result<(), utils::errno::Error> {
        Ok(())
    }
    /// This function will be called on microVm teardown.
    fn on_vmm_stop(&mut self) -> std::result::Result<(), utils::errno::Error> {
        Ok(())
    }
}

/// Shorthand result type for internal VMM commands.
pub type Result<T> = std::result::Result<T, Error>;

/// Contains the state and associated methods required for the Firecracker VMM.
pub struct Vmm {
    events_observer: Option<Box<dyn VmmEventsObserver>>,

    // Guest VM core resources.
    guest_memory: GuestMemoryMmap,

    kernel_cmdline: KernelCmdline,

    vcpus_handles: Vec<VcpuHandle>,
    exit_evt: EventFd,
    vm: Vm,

    // Guest VM devices.
    mmio_device_manager: MMIODeviceManager,
    #[cfg(target_arch = "x86_64")]
    pio_device_manager: PortIODeviceManager,
}

impl Vmm {
    /// Gets the the specified bus device.
    pub fn get_bus_device(
        &self,
        device_type: DeviceType,
        device_id: &str,
    ) -> Option<&Mutex<dyn BusDevice>> {
        self.mmio_device_manager.get_device(device_type, device_id)
    }

    /// Starts the microVM vcpus.
    pub fn start_vcpus(
        &mut self,
        mut vcpus: Vec<Vcpu>,
        vmm_seccomp_filter: BpfProgram,
        vcpu_seccomp_filter: BpfProgramRef,
    ) -> Result<()> {
        let vcpu_count = vcpus.len();

        if let Some(observer) = self.events_observer.as_mut() {
            observer.on_vmm_boot().map_err(Error::VmmObserverInit)?;
        }

        Vcpu::register_kick_signal_handler();

        self.vcpus_handles.reserve(vcpu_count as usize);

        for mut vcpu in vcpus.drain(..) {
            vcpu.set_mmio_bus(self.mmio_device_manager.bus.clone());

            self.vcpus_handles.push(
                vcpu.start_threaded(vcpu_seccomp_filter.to_vec())
                    .map_err(Error::VcpuHandle)?,
            );
        }

        // Load seccomp filters for the VMM thread.
        // Execution panics if filters cannot be loaded, use --seccomp-level=0 if skipping filters
        // altogether is the desired behaviour.
        SeccompFilter::apply(vmm_seccomp_filter).map_err(Error::SeccompFilters)?;

        // The vcpus start off in the `Paused` state, let them run.
        self.resume_vcpus()?;

        Ok(())
    }

    /// Sends a resume command to the vcpus.
    pub fn resume_vcpus(&mut self) -> Result<()> {
        for handle in self.vcpus_handles.iter() {
            handle
                .send_event(VcpuEvent::Resume)
                .map_err(Error::VcpuEvent)?;
        }
        for handle in self.vcpus_handles.iter() {
            match handle
                .response_receiver()
                .recv_timeout(Duration::from_millis(1000))
            {
                Ok(VcpuResponse::Resumed) => (),
                _ => return Err(Error::VcpuResume),
            }
        }
        Ok(())
    }

    /// Configures the system for boot.
    pub fn configure_system(&self, vcpus: &[Vcpu], initrd: &Option<InitrdConfig>) -> Result<()> {
        #[cfg(target_arch = "x86_64")]
        arch::x86_64::configure_system(
            &self.guest_memory,
            vm_memory::GuestAddress(arch::x86_64::layout::CMDLINE_START),
            self.kernel_cmdline.len() + 1,
            initrd,
            vcpus.len() as u8,
        )
        .map_err(Error::ConfigureSystem)?;

        #[cfg(target_arch = "aarch64")]
        {
            let vcpu_mpidr = vcpus.into_iter().map(|cpu| cpu.get_mpidr()).collect();
            arch::aarch64::configure_system(
                &self.guest_memory,
                &self
                    .kernel_cmdline
                    .as_cstring()
                    .map_err(Error::LoadCommandline)?,
                vcpu_mpidr,
                self.mmio_device_manager.get_device_info(),
                self.vm.get_irqchip(),
                initrd,
            )
            .map_err(Error::ConfigureSystem)?;
        }
        Ok(())
    }

    /// Returns a reference to the inner `GuestMemoryMmap` object if present, or `None` otherwise.
    pub fn guest_memory(&self) -> &GuestMemoryMmap {
        &self.guest_memory
    }

    /// Injects CTRL+ALT+DEL keystroke combo in the i8042 device.
    #[cfg(target_arch = "x86_64")]
    pub fn send_ctrl_alt_del(&mut self) -> Result<()> {
        self.pio_device_manager
            .i8042
            .lock()
            .expect("i8042 lock was poisoned")
            .trigger_ctrl_alt_del()
            .map_err(Error::I8042Error)
    }

    /// Waits for all vCPUs to exit and terminates the Firecracker process.
    pub fn stop(&mut self, exit_code: i32) {
        info!("Vmm is stopping.");

        if let Some(observer) = self.events_observer.as_mut() {
            if let Err(e) = observer.on_vmm_stop() {
                warn!("{}", Error::VmmObserverTeardown(e));
            }
        }

        // Log the metrics before exiting.
        if let Err(e) = LOGGER.log_metrics() {
            error!("Failed to log metrics while stopping: {}", e);
        }

        // Exit from Firecracker using the provided exit code. Safe because we're terminating
        // the process anyway.
        unsafe {
            libc::_exit(exit_code);
        }
    }

    fn log_boot_time(t0_ts: &TimestampUs) {
        let now_tm_us = TimestampUs::default();

        let boot_time_us = now_tm_us.time_us - t0_ts.time_us;
        let boot_time_cpu_us = now_tm_us.cputime_us - t0_ts.cputime_us;
        info!(
            "Guest-boot-time = {:>6} us {} ms, {:>6} CPU us {} CPU ms",
            boot_time_us,
            boot_time_us / 1000,
            boot_time_cpu_us,
            boot_time_cpu_us / 1000
        );
    }

    /// Returns a reference to the inner KVM Vm object.
    pub fn kvm_vm(&self) -> &Vm {
        &self.vm
    }
}

impl Subscriber for Vmm {
    /// Handle a read event (EPOLLIN).
    fn process(&mut self, event: &EpollEvent, _: &mut EventManager) {
        let source = event.fd();
        let event_set = event.event_set();

        if source == self.exit_evt.as_raw_fd() && event_set == EventSet::IN {
            let _ = self.exit_evt.read();
            // Query each vcpu for the exit_code.
            // If the exit_code can't be found on any vcpu, it means that the exit signal
            // has been issued by the i8042 controller in which case we exit with
            // FC_EXIT_CODE_OK.
            let exit_code = self
                .vcpus_handles
                .iter()
                .find_map(|handle| match handle.response_receiver().try_recv() {
                    Ok(VcpuResponse::Exited(exit_code)) => Some(exit_code),
                    _ => None,
                })
                .unwrap_or(FC_EXIT_CODE_OK);
            self.stop(i32::from(exit_code));
        } else {
            error!("Spurious EventManager event for handler: Vmm");
        }
    }

    fn interest_list(&self) -> Vec<EpollEvent> {
        vec![EpollEvent::new(
            EventSet::IN,
            self.exit_evt.as_raw_fd() as u64,
        )]
    }
}

#[cfg(test)]
mod tests {}
