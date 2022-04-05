extern crate log;
extern crate simplelog;

use simplelog::*;
use tokio::runtime::Builder;

mod filter;
mod input;
mod output;
mod utils;

use crate::output::{player, write_hls};
use crate::utils::{init_config, init_logging, validate_ffmpeg, run_rpc, GlobalConfig, ProcessControl};

fn main() {
    init_config();
    let config = GlobalConfig::global();
    let proc_control = ProcessControl::new();

    let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
    let rt_handle = runtime.handle();

    let logging = init_logging(rt_handle.clone(), proc_control.is_terminated.clone());
    CombinedLogger::init(logging).unwrap();

    validate_ffmpeg();

    if config.rpc_server.enable {
        rt_handle.spawn(run_rpc(proc_control.clone()));
    }

    if config.out.mode.to_lowercase() == "hls".to_string() {
        write_hls(rt_handle, proc_control.is_terminated.clone());
    } else {
        player(rt_handle, proc_control);
    }

    info!("Playout done...");
}
