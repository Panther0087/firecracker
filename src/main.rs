extern crate backtrace;
#[macro_use(crate_version, crate_authors)]
extern crate clap;

extern crate api_server;
extern crate data_model;
#[macro_use]
extern crate logger;
extern crate seccomp;
extern crate vmm;

use backtrace::Backtrace;
use clap::{App, Arg};
use std::panic;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::sync::{Arc, RwLock};

use api_server::request::instance_info::{InstanceInfo, InstanceState};
use api_server::ApiServer;
use data_model::mmds::MMDS;
use logger::{Metric, LOGGER, METRICS};

const DEFAULT_API_SOCK_PATH: &str = "/tmp/firecracker.socket";

fn main() {
    // If the signal handler can't be set, it's OK to panic.
    seccomp::setup_sigsys_handler().expect("Failed to register signal handler");
    // Start firecracker by setting up a panic hook, which will be called before
    // terminating as we're building with panic = "abort".
    // It's worth noting that the abort is caused by sending a SIG_ABORT signal to the process.
    panic::set_hook(Box::new(move |info| {
        // We're currently using the closure parameter, which is a &PanicInfo, for printing the
        // origin of the panic, including the payload passed to panic! and the source code location
        // from which the panic originated.
        error!("Panic occurred: {:?}", info);
        METRICS.vmm.panic_count.inc();

        let bt = Backtrace::new();
        error!("{:?}", bt);

        // Log the metrics before aborting.
        if let Err(e) = LOGGER.log_metrics() {
            error!("Failed to log metrics on abort. {}:?", e);
        }
    }));

    let cmd_arguments = App::new("firecracker")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Launch a microvm.")
        .arg(
            Arg::with_name("api_sock")
                .long("api-sock")
                .help("Path to unix domain socket used by the API")
                .default_value(DEFAULT_API_SOCK_PATH)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("jailed")
                .long("jailed")
                .help("Let Firecracker know it's running inside a jail."),
        )
        .get_matches();

    let bind_path = cmd_arguments
        .value_of("api_sock")
        .map(|s| PathBuf::from(s))
        .unwrap();

    if cmd_arguments.is_present("jailed") {
        data_model::FIRECRACKER_IS_JAILED.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    let shared_info = Arc::new(RwLock::new(InstanceInfo {
        state: InstanceState::Uninitialized,
    }));
    let mmds_info = MMDS.clone();
    let (to_vmm, from_api) = channel();
    let server = ApiServer::new(mmds_info, shared_info.clone(), to_vmm).unwrap();

    let api_event_fd = server
        .get_event_fd_clone()
        .expect("Cannot clone API eventFD.");
    let _vmm_thread_handle = vmm::start_vmm_thread(shared_info, api_event_fd, from_api);

    server.bind_and_run(bind_path).unwrap();
}

#[cfg(test)]
mod tests {
    extern crate tempfile;

    use self::tempfile::NamedTempFile;
    use super::*;

    use std::fs::File;
    use std::io::BufRead;
    use std::io::BufReader;
    use std::path::Path;
    use std::time::Duration;
    use std::{fs, thread};

    struct EnterDrop {
        log_file: String,
    }

    impl Drop for EnterDrop {
        fn drop(&mut self) {
            validate_backtrace(
                &self.log_file,
                &[
                    // This is the assertion string. Making sure that stack backtrace is outputted
                    // upon panic.
                    ("[ERROR", "main.rs", "Panic occurred"),
                    ("[ERROR", "main.rs", "stack backtrace:"),
                    ("0:", "0x", "backtrace::"),
                ],
            );
            fs::remove_file(DEFAULT_API_SOCK_PATH).expect("Failure in removing socket file.");
        }
    }

    fn validate_backtrace(log_path: &str, expected: &[(&'static str, &'static str, &'static str)]) {
        let f = File::open(log_path).unwrap();
        let mut reader = BufReader::new(f);

        let mut line = String::new();
        for tuple in expected {
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert!(line.contains(&tuple.0));
            assert!(line.contains(&tuple.1));
            assert!(line.contains(&tuple.2));
        }
    }

    #[test]
    #[should_panic(expected = "Test that panic outputs backtrace")]
    fn test_main() {
        // There is no reason to run this test if the default socket path exists.
        assert!(!Path::new(DEFAULT_API_SOCK_PATH).exists());

        let log_file_temp =
            NamedTempFile::new().expect("Failed to create temporary output logging file.");
        let metrics_file_temp =
            NamedTempFile::new().expect("Failed to create temporary metrics logging file.");
        let log_file = String::from(log_file_temp.path().to_path_buf().to_str().unwrap());

        thread::spawn(|| {
            main();
        });

        const MAX_WAIT_ITERS: u32 = 20;
        let mut iter_count = 0;
        loop {
            thread::sleep(Duration::from_secs(1));
            if Path::new(DEFAULT_API_SOCK_PATH).exists() {
                LOGGER
                    .init(
                        Some(log_file_temp.path().to_str().unwrap().to_string()),
                        Some(metrics_file_temp.path().to_str().unwrap().to_string()),
                    )
                    .expect("Could not initialize logger.");
                // EnterDrop is used here only to be able to check the content of the log file
                // after panicking.
                let _my_setup = EnterDrop { log_file };

                // This string argument has to match the one from the should_panic above.
                panic!("Test that panic outputs backtrace");
            }
            iter_count += 1;
            if iter_count > MAX_WAIT_ITERS {
                fs::remove_file(DEFAULT_API_SOCK_PATH).expect("failure in removing socket file");
                assert!(false);
            }
        }
    }
}
