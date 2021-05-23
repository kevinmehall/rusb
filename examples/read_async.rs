use rusb::{AsyncPool, Context, UsbContext};

use std::str::FromStr;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 4 {
        eprintln!("Usage: read_async <vendor-id> <product-id> <endpoint>");
        return;
    }

    let vid: u16 = FromStr::from_str(args[1].as_ref()).unwrap();
    let pid: u16 = FromStr::from_str(args[2].as_ref()).unwrap();
    let endpoint: u8 = FromStr::from_str(args[3].as_ref()).unwrap();

    let ctx = Context::new().expect("Could not initialize libusb");
    let device = ctx
        .open_device_with_vid_pid(vid, pid)
        .expect("Could not find device");

    const NUM_TRANSFERS: usize = 32;
    const BUF_SIZE: usize = 64;

    let mut async_pool =
        AsyncPool::new_bulk(device, endpoint).expect("Failed to create async pool!");

    while async_pool.pending() < NUM_TRANSFERS {
        async_pool.submit(Vec::with_capacity(BUF_SIZE)).expect("Failed to submit transfer");
    }

    let timeout = Duration::from_secs(10);
    loop {
        let data = async_pool.poll(timeout).expect("Transfer failed");
        println!("Got data: {} {:?}", data.len(), data);
        async_pool.submit(data).expect("Failed to resubmit transfer");
    }
}
