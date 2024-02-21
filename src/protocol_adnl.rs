use std::fs::File;
use std::io::Write;
use std::sync::mpsc::SyncSender;
use std::{
    io::{self, BufReader, Read},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use bzip2::bufread::MultiBzDecoder;
use regex::Regex;
use rusb::{Device, GlobalContext};

use crate::{protocol::Handle, USB_VID_AMLOGIC};

// IN is device to host
const ADNL_IN_EP: u8 = 0x81;
// OUT is host to device
const ADNL_OUT_EP: u8 = 0x01;

// for ease of distribution, let's include the bins directly
const UBOOT_BIN_SIGNED: &[u8] = include_bytes!("../u-boot/u-boot.bin.signed");
const UBOOT_USB_BIN_SIGNED: &[u8] = include_bytes!("../u-boot/u-boot.bin.usb.signed");

#[allow(unused)]
const UBOOT_BIN_FORCE_FLASH_SIGNED: &[u8] = include_bytes!("../u-boot/u-boot.bin.flashonly.signed");

#[allow(unused)]
enum CmdAdnl {
    Devices,
    /// Partition num, offset, length
    OemMreadInit(u8, u64, u64),
    OemMreadRequest,
    OemMreadUpload,
    OemMreadFinish,

    /// Partition num, offset, length
    OemMwriteInit(u8, u64, u64),
    OemMwriteRequest,
}

impl From<CmdAdnl> for String {
    fn from(value: CmdAdnl) -> Self {
        match value {
            CmdAdnl::Devices => "getvar:downloadsize".into(),
            CmdAdnl::OemMreadInit(part, offset, len) => {
                format!("oem mread {len} normal mmc {part} {offset}")
            }
            CmdAdnl::OemMreadRequest => "mread:status=request".into(),
            CmdAdnl::OemMreadUpload => "mread:status=upload".into(),
            CmdAdnl::OemMreadFinish => "mread:status=finish".into(),

            CmdAdnl::OemMwriteInit(part, offset, len) => {
                format!("oem mwrite {len} normal mmc {part} {offset}")
            }
            CmdAdnl::OemMwriteRequest => "mwrite".into(),
        }
    }
}

pub enum OemWriteType<'a> {
    File(&'a str),
    Raw(&'a [u8]),
    Bzip2(&'a str),
}

// this just aggregates the data from the chunker thread to ensure
// there is always 0x20000 bytes of data to write until the last chunk
// or if the data is smaller, that works too
fn aggregator_thread(rx: Receiver<Vec<u8>>, tx2: Sender<Vec<u8>>) {
    let mut working_set: Vec<u8> = Vec::new();
    let mut is_done = false;
    while let Ok(buf) = rx.recv() {
        if buf.is_empty() {
            is_done = true;
        } else {
            let mut tvec: Vec<u8> = Vec::new();
            buf.as_slice()
                .read_to_end(&mut tvec)
                .expect("Failed to read data in thread");
            working_set.append(&mut tvec);
        }

        if working_set.len() >= 0x20000 {
            let mut to_drop = 0;
            for (ix, ch) in working_set.chunks_exact(0x20000).enumerate() {
                tx2.send(ch.to_vec()).unwrap();
                to_drop = ix + 1;
            }
            // drop what we just sent down the pipe
            working_set.drain(0..(to_drop * 0x20000));
        } else if is_done && working_set.len() < 0x20000 {
            tx2.send(working_set.clone()).unwrap();
            working_set.clear();
            tx2.send(Vec::new()).unwrap();
            break;
        }
    }
}

fn do_write_data(h: &Handle, offset: u64, data: OemWriteDataType, tx: mpsc::Sender<usize>) {
    let mut offset = offset;

    let (tx1, rx1) = mpsc::sync_channel::<Vec<u8>>(1);
    let (tx2, rx2) = mpsc::channel::<Vec<u8>>();

    let t1 = std::thread::spawn(move || aggregator_thread(rx1, tx2));
    std::thread::spawn(move || chunker_thread(data, tx1));
    loop {
        match rx2.try_recv() {
            Ok(data) => {
                if data.is_empty() {
                    break;
                }
                do_write_blk_cmd(
                    h,
                    String::from(CmdAdnl::OemMwriteInit(1, offset, data.len() as u64)).as_bytes(),
                )
                .unwrap();
                do_read_bulk(h).unwrap();
                do_write_blk_cmd(h, String::from(CmdAdnl::OemMwriteRequest).as_bytes()).unwrap();
                do_read_bulk(h).unwrap();
                do_write_blk_cmd(h, data.as_slice()).expect("Failed to write data");
                do_read_bulk(h).unwrap();
                tx.send(data.len()).ok();
                offset += data.len() as u64;
            }
            Err(_e) => {}
        }
    }
    drop(tx);
    t1.join().unwrap();
}

fn progress_thread(rx: Receiver<usize>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut total = 0;
        let now = Instant::now();
        while let Ok(by) = rx.recv() {
            total += by;
            let n = now.elapsed();
            print!(
                "\rWrote {:>10}, Elapsed {:<03.3} seconds",
                total,
                n.as_millis() as f64 / 1000.0
            );
            io::stdout().flush().ok();
        }
        println!();
    })
}

enum OemWriteDataType {
    BzDecoder(MultiBzDecoder<BufReader<File>>),
    File(BufReader<File>),
    Buff(Vec<u8>),
}

impl Read for OemWriteDataType {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            OemWriteDataType::BzDecoder(d) => d.read(buf),
            OemWriteDataType::File(d) => d.read(buf),
            OemWriteDataType::Buff(d) => match d.as_slice().read(buf) {
                Ok(l) => {
                    d.drain(0..l);
                    Ok(l)
                }
                Err(e) => Err(e),
            },
        }
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        match self {
            OemWriteDataType::BzDecoder(d) => d.read_exact(buf),
            OemWriteDataType::File(d) => d.read_exact(buf),
            OemWriteDataType::Buff(d) => {
                let l = buf.len();
                match d.as_slice().read_exact(buf) {
                    Ok(()) => {
                        d.drain(0..l);
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }
}

// this loop chunks the bzip2 data into the expected 0x20000 byte writeable chunks
// for the adnl protocol, the bzip2 threads ensures that we do not try to write anything
// until we have a full 0x20000 chunk(s), this with the offset being intervals of 0x20000 appear to
// be required, though it's not clear exactly why
fn chunker_thread(mut data: OemWriteDataType, notify_tx: SyncSender<Vec<u8>>) {
    let mut buf = [0u8; 0x20000];
    loop {
        if let Ok(len) = data.read(&mut buf) {
            if len > 0 {
                let mut dc = Vec::new();
                let _ = dc.write(&buf[0..len]).expect("Failed to write bytes");
                notify_tx.send(dc).ok();
                // this helps keep memory consumption low
                // and doens't impact overall time as the long pole
                // is writing out the data to the device
                std::thread::sleep(Duration::from_millis(10));
            } else {
                notify_tx.send(Vec::new()).ok();
                break;
            };
        }
    }
}

pub fn oem_mwrite(h: &Handle, offset: u64, input: OemWriteType) {
    use bzip2::bufread;
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    let progress = progress_thread(rx);
    let data = match input {
        OemWriteType::Bzip2(file) => {
            let buf = BufReader::new(std::fs::File::open(PathBuf::from(file)).unwrap());
            let bzd = bufread::MultiBzDecoder::new(buf);
            OemWriteDataType::BzDecoder(bzd)
        }
        OemWriteType::File(file) => {
            let buf = BufReader::new(std::fs::File::open(PathBuf::from(file)).unwrap());
            OemWriteDataType::File(buf)
        }
        OemWriteType::Raw(data) => OemWriteDataType::Buff(data.to_owned()),
    };

    do_write_data(h, offset, data, tx);
    progress.join().expect("Failed to 'join' progress handle");
}

pub fn oem_mread(h: &Handle, offset: u64, len: u64) {
    let timeout = Duration::from_millis(3000);
    if let Ok(len) = h.write_bulk(
        ADNL_OUT_EP,
        String::from(CmdAdnl::OemMreadInit(1, offset, len)).as_bytes(),
        timeout,
    ) {
        println!("Wrote {len} bytes!");
    } else {
        println!("Failed to write");
        return;
    }
    let mut buf = [0u8; 8192];
    match h.read_bulk(ADNL_IN_EP, &mut buf, timeout) {
        Ok(len) => {
            println!("Read {len} bytes!");
            println!("{}", String::from_utf8_lossy(&buf));
        }
        Err(e) => println!("Failed to read: {:?}", e),
    }
    h.write_bulk(
        ADNL_OUT_EP,
        String::from(CmdAdnl::OemMreadRequest).as_bytes(),
        timeout,
    )
    .ok();
    h.read_bulk(ADNL_IN_EP, &mut buf, timeout).ok();
    println!("{}", String::from_utf8_lossy(&buf));

    h.write_bulk(
        ADNL_OUT_EP,
        String::from(CmdAdnl::OemMreadUpload).as_bytes(),
        timeout,
    )
    .ok();
    if let Ok(rlen) = h.read_bulk(ADNL_IN_EP, &mut buf, timeout) {
        println!("=== Read {rlen} Bytes ===");
        let mut ix = 0;
        let mut cnt = 0;
        let mut addr = offset;
        while ix < rlen {
            if cnt == 0 {
                print!("{:08x}: ", addr);
                addr += 16;
            }
            let s = format!("{:02x}", buf[ix]);
            print!("{s} ");
            ix += 1;
            cnt += 1;
            if cnt > 15 {
                println!();
                cnt = 0;
            }
        }
    }
    println!("=== ===");

    // h.write_bulk(ADNL_OUT_EP, String::from(CmdAdnl::OemMreadFinish).as_bytes(), timeout).ok();
    // h.read_bulk(ADNL_IN_EP, &mut buf, timeout).ok();
    // println!("{}", buf.len());
}

pub fn devices(_h: &Handle) {
    // check_in_mode(h, BootMode::Bl1).unwrap();
    // check_in_mode(h, BootMode::Bl2).unwrap();
}

fn do_read_bulk(h: &Handle) -> Result<Vec<u8>, String> {
    let timeout = Duration::from_millis(3000);
    let mut buf = [0u8; 512];
    let result = match h.read_bulk(ADNL_IN_EP, &mut buf, timeout) {
        Ok(len) => {
            let s = String::from_utf8_lossy(&buf);
            // get the first item as responses are a bit strange from the device (and is probably a bug)
            // OKAY0x3F800<nul>max-download-size<nul>serialno<nul>product<nul>AMLOGIC<nul>identify<nul>getc
            if let Some(r) = s.split('\0').next() {
                let re = Regex::new(r"(?<status>(OKAY|FAIL|DATA))(?<msg>.*)").unwrap();
                match re.captures(r) {
                    Some(cap) => match &cap["status"] {
                        "OKAY" | "DATA" => Ok(buf[4..len].to_vec()),
                        "FAIL" => Err(String::from(&cap["msg"])),
                        _ => Err("Unknown response".into()),
                    },
                    None => {
                        println!("Failed to find expected response: '{r}'!");
                        Err("Regular expression failed to match".into())
                    }
                }
            } else {
                println!("Failed to get expected response: '{}'", s);
                Err("Failed to get a valid response".into())
            }
        }
        Err(e) => {
            println!("Failed to read: {:?}", e);
            Err("Failed to read".into())
        }
    };
    result
}

enum BulkCommand<'a> {
    String(&'a str),
    Raw(&'a [u8]),
}

impl<'b, 'a> From<&'b str> for BulkCommand<'a>
where
    'b: 'a,
{
    fn from(value: &'b str) -> Self {
        BulkCommand::String(value)
    }
}

impl<'b, 'a> From<&'b [u8]> for BulkCommand<'a>
where
    'b: 'a,
{
    fn from(value: &'b [u8]) -> Self {
        BulkCommand::Raw(value)
    }
}

fn do_write_blk_cmd<'a>(h: &Handle, cmd: impl Into<BulkCommand<'a>>) -> Result<usize, String> {
    let timeout = Duration::from_millis(3000);

    let buf = match cmd.into() {
        BulkCommand::String(s) => {
            // println!("Sending command: '{}'", s);
            s.as_bytes()
        }
        BulkCommand::Raw(b) => b,
    };

    h.write_bulk(ADNL_OUT_EP, buf, timeout).map_err(|e| {
        println!("Failed to send command:  {}", e);
        "Failed to send command".to_string()
    })
}

pub fn do_bootloader_flash(h: &Handle) -> Result<Device<GlobalContext>, String> {
    let data = UBOOT_USB_BIN_SIGNED;

    //// bl1_boot -f uboot.bin.usb.signed
    check_in_mode(h, BootMode::Bl1).expect("Should be in BL1 mode");
    check_in_mode(h, BootMode::Bl1).expect("Should be in BL1 mode");

    do_write_blk_cmd(h, "getvar:getchipinfo-1").unwrap();
    do_read_bulk(h).unwrap();

    let mut dlsize = get_download_size(h).expect("Failed to get download size");

    do_write_blk_cmd(h, format!("download:{:08X}", dlsize).as_str()).unwrap();
    do_read_bulk(h).unwrap();

    // write the data now
    for ch in data[0..dlsize as usize].chunks(0x4000_usize) {
        do_write_blk_cmd(h, ch).expect("Failed to write chunk data");
    }
    do_read_bulk(h).unwrap();

    do_write_blk_cmd(h, "boot").unwrap();
    do_read_bulk(h).unwrap();

    // wait for the device to boot again
    std::thread::sleep(Duration::from_millis(500));
    check_in_mode(h, BootMode::Bl2).expect("Should be in BL2 mode");
    // this next part is 'reveresed engineered from a USB trace of the adnl tool
    // so it could be...fragile
    do_write_blk_cmd(h, "getvar:cbw").unwrap();
    do_read_bulk(h).unwrap();

    let data = UBOOT_BIN_SIGNED;

    struct WriteDef {
        addr: u32,
        size: usize,
        last: bool,
        mode: BootMode,
    }

    let offsets = [
        WriteDef {
            addr: 0x64000,
            size: 0x9600,
            last: false,
            mode: BootMode::Bl2,
        },
        WriteDef {
            addr: 0x8c000,
            size: 0x9600,
            last: false,
            mode: BootMode::Bl2,
        },
        WriteDef {
            addr: 0x96000,
            size: 0x9600,
            last: false,
            mode: BootMode::Bl2,
        },
        WriteDef {
            addr: 0x6e000,
            size: 0x9600,
            last: false,
            mode: BootMode::Bl2,
        },
        WriteDef {
            addr: 0x78000,
            size: 0x9600,
            last: false,
            mode: BootMode::Bl2,
        },
        WriteDef {
            addr: 0x82000,
            size: 0x9600,
            last: false,
            mode: BootMode::Bl2,
        },
        WriteDef {
            addr: 0x42000,
            size: 0x11000,
            last: false,
            mode: BootMode::Bl2,
        },
        WriteDef {
            addr: 0x53000,
            size: 0x11000,
            last: false,
            mode: BootMode::Bl2e,
        },
        WriteDef {
            addr: 0xa4000,
            size: 0x8000,
            last: false,
            mode: BootMode::Bl2e,
        },
        WriteDef {
            addr: 0xac000,
            size: 0x26C260,
            last: true,
            mode: BootMode::Bl2e, // this one is not actually checked/used
        },
    ]
    .into_iter();

    for wd in offsets {
        dlsize = 0x2000;
        let i = wd.addr as usize;
        let l = wd.size + wd.addr as usize;
        let dsl = &data[i..l];

        let mut csum = AdnlChecksum::new();
        for ch in dsl.chunks(dlsize as usize) {
            csum.update(ch);
            do_write_blk_cmd(h, format!("download:{:08X}", ch.len()).as_str()).unwrap();
            do_read_bulk(h).unwrap();
            do_write_blk_cmd(h, ch).expect("Failed to write bytes");
            do_read_bulk(h).unwrap();
        }

        do_write_blk_cmd(h, "setvar:checksum").unwrap();
        do_read_bulk(h).unwrap();
        let buf = csum.get_csum().to_le_bytes();
        do_write_blk_cmd(h, buf.as_ref()).expect("Failed to write checksum");
        do_read_bulk(h).unwrap();
        do_write_blk_cmd(h, "getvar:cbw").unwrap();
        do_read_bulk(h).unwrap();

        // this breaks the flow on the last iteration as this the previous
        // commmand appears to trigger bl33 to boot
        if !wd.last {
            // do_write_blk_cmd(h, "getvar:identify").unwrap();
            // do_read_bulk(h).unwrap();
            check_in_mode(h, wd.mode).unwrap();
            do_write_blk_cmd(h, "getvar:cbw").unwrap();
            do_read_bulk(h).unwrap();
        }
    }

    // the device will 'boot' and reconnect as a different USB device number
    Ok(wait_for_device_reconnect().expect("Failed to find device after reconnect!"))
}

pub fn erase_emmc(h: &Handle) -> Result<Device<GlobalContext>, String> {
    let dev = do_bootloader_flash(h).expect("Failed to flash bootloader(s)");
    let handle = dev.open().expect("Failed to open usb device");
    let h = &handle;

    // this will wipe the boot partitions
    do_write_blk_cmd(h, "oem disk_initial 1").unwrap();
    do_read_bulk(h).unwrap();

    // this will format the boot partition, located
    // 4MB from the 'start' of emmc, per the wic file
    let data = [0u8; 102400];
    oem_mwrite(h, 8192 * 512, OemWriteType::Raw(&data));

    // for good measure, blow away the mbr too
    oem_mwrite(h, 0, OemWriteType::Raw(&data));

    // reflash the bootloader we just erased to boot into adnl mode
    let data = UBOOT_BIN_SIGNED;

    let dlsize = data.len() as u32;
    let mut csum = AdnlChecksum::new();

    // now write the entire file to flash/boot partition
    // important bits duped from `do_flash` below...
    do_write_blk_cmd(h, "oem disk_initial 1").unwrap();
    do_read_bulk(h).unwrap();
    do_write_blk_cmd(
        h,
        format!("oem mwrite 0x{:08X} normal store bootloader", dlsize).as_str(),
    )
    .unwrap();
    do_read_bulk(h).unwrap();
    do_write_blk_cmd(h, "mwrite:verify=addsum").unwrap();
    do_read_bulk(h).unwrap();

    for ch in data.chunks(0x4000) {
        csum.update(ch);
        do_write_blk_cmd(h, ch).expect("Failed to write chunk!");
    }
    let buf = csum.get_csum().to_le_bytes();
    do_write_blk_cmd(h, buf.as_ref()).expect("Failed to write checksum");
    do_read_bulk(h).unwrap();

    Ok(dev)
}

pub fn do_flash(h: &Handle) -> Result<Device<GlobalContext>, String> {
    let dev = do_bootloader_flash(h).expect("Failed to flash bootloader(s)");
    let handle = dev.open().expect("Failed to open usb device");
    let h = &handle;

    let data = UBOOT_BIN_SIGNED;

    let dlsize = data.len() as u32;
    let mut csum = AdnlChecksum::new();
    // now write the entire file to flash/boot partition
    do_write_blk_cmd(h, "oem disk_initial 1").unwrap();
    do_read_bulk(h).unwrap();
    do_write_blk_cmd(
        h,
        format!("oem mwrite 0x{:08X} normal store bootloader", dlsize).as_str(),
    )
    .unwrap();
    do_read_bulk(h).unwrap();
    do_write_blk_cmd(h, "mwrite:verify=addsum").unwrap();
    do_read_bulk(h).unwrap();

    for ch in data.chunks(0x4000) {
        csum.update(ch);
        do_write_blk_cmd(h, ch).expect("Failed to write chunk!");
    }
    let buf = csum.get_csum().to_le_bytes();
    do_write_blk_cmd(h, buf.as_ref()).expect("Failed to write checksum");
    do_read_bulk(h).unwrap();

    do_write_blk_cmd(h, "reboot").unwrap();
    do_read_bulk(h).unwrap();

    Ok(wait_for_device_reconnect().expect("Failed to detect device"))
}

fn find_usb_device() -> Result<Device<GlobalContext>, String> {
    if let Some(dev) = rusb::devices().unwrap().iter().find(|dev| {
        let des = dev.device_descriptor().unwrap();
        let vid = des.vendor_id();
        let pid = des.product_id();

        vid == USB_VID_AMLOGIC && matches!(pid, crate::USB_PID_AML_DNL)
    }) {
        Ok(dev)
    } else {
        Err("Not Found".into())
    }
}

fn wait_for_device_reconnect() -> Result<Device<GlobalContext>, String> {
    // wait for this device to leave
    let max = Duration::from_secs(30);
    let now = Instant::now();
    let curr_dev = find_usb_device().expect("Failed to find adnl device");
    println!("Searching for Amlogic USB devices...");
    while now.elapsed() < max {
        let left = 30 - now.elapsed().as_secs();
        print!("Remaing time: {:<3}s\r", left);
        std::io::stdout().flush().ok();
        if let Ok(dev) = find_usb_device() {
            if curr_dev.address() == dev.address() {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            } else {
                std::thread::sleep(Duration::from_millis(250));
                let ds = dev.device_descriptor().expect("Failed to get descriptor");
                let vid = ds.vendor_id();
                let pid = ds.product_id();
                println!(
                    "\nFound {vid:04x}:{pid:04x} on bus {:03}, device {:03}",
                    dev.bus_number(),
                    dev.address(),
                );
                return Ok(dev);
            }
        } else {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Err("Failed to find device".into())
}

pub fn device_reboot(h: &Handle) {
    do_write_blk_cmd(h, "reboot").unwrap();
    do_read_bulk(h).unwrap();
}

#[derive(Debug, PartialEq)]
pub enum BootMode {
    Bl1,
    Bl2,
    Bl2e, // some undefined intermediate mode I guess
    Tpl,

    Invalid,
}

impl From<Vec<u8>> for BootMode {
    fn from(value: Vec<u8>) -> Self {
        if value.len() < 4 {
            return BootMode::Invalid;
        }

        match value[0..4] {
            [6, 0, 0, 0] => BootMode::Bl1,
            [6, 0, 0, 8] => BootMode::Bl2,
            [6, 0, 0, 12] => BootMode::Bl2e,
            [6, 0, 0, 16] => BootMode::Tpl,
            _ => BootMode::Invalid,
        }
    }
}

fn identify(h: &Handle) -> BootMode {
    do_write_blk_cmd(h, "getvar:identify").unwrap();
    let mode = do_read_bulk(h).unwrap();
    BootMode::from(mode)
}

pub fn check_in_mode(h: &Handle, expected: BootMode) -> Result<(), String> {
    let mode = identify(h);
    if mode != expected {
        return Err(format!("Expected Mode '{:?}', Was in {:?}", expected, mode));
    }
    Ok(())
}

fn get_download_size(h: &Handle) -> Result<u32, String> {
    do_write_blk_cmd(h, "getvar:downloadsize").unwrap();
    match do_read_bulk(h) {
        Ok(msg) => {
            let m = String::from_utf8_lossy(&msg);
            let s = m.split('\0').next().unwrap();
            Ok(u32::from_str_radix(s.to_lowercase().replace("0x", "").as_str(), 16).unwrap())
        }
        Err(e) => {
            println!("Failed to get download size: {}", e);
            Err("Failed to get download size".to_owned())
        }
    }
}

struct AdnlChecksum {
    sum: u64,
    unaligned: Vec<u8>,
}

impl AdnlChecksum {
    fn new() -> Self {
        AdnlChecksum {
            sum: 0,
            unaligned: Vec::new(),
        }
    }

    pub fn update(&mut self, incoming: &[u8]) {
        let incoming_len = incoming.len();
        let curr_len = self.unaligned.len();

        let data = if curr_len > 1 {
            self.unaligned.clone().append(&mut incoming.to_vec());
            self.unaligned.clone()
        } else {
            incoming.to_vec()
        };

        let rb = (curr_len + incoming_len) % 4;

        #[allow(unused_assignments)]
        let mut need_compute = Vec::new();
        (need_compute, self.unaligned) = if rb > 0 {
            let (l, r) = self.unaligned.split_at(rb);
            (l.to_vec(), r.to_vec())
        } else {
            (data, Vec::new())
        };

        let mut sum: u64 = 0;
        for ch in need_compute.chunks(4) {
            sum += u32::from_le_bytes(ch.try_into().unwrap()) as u64;
        }
        self.sum += sum;
    }

    pub fn get_csum(&self) -> u64 {
        self.sum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_csum() {
        let mut csum = AdnlChecksum::new();

        let data = [5u8; 512];
        csum.update(&data);
        csum.update(&data);
        println!("Final sum {:08X}", csum.get_csum());
    }
}
