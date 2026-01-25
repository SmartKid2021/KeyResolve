use anyhow::{Context, Result};
use evdev::{Device, EventType, InputEvent, KeyCode, uinput::VirtualDevice};
use nix::poll::{PollFd, PollFlags, poll};
use std::{
    io::Write,
    os::fd::{AsRawFd, BorrowedFd},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

fn main() -> Result<()> {
    let keyboards = enumerate_keyboards()?;
    let selected = select_keyboard(&keyboards)?;

    println!("Waiting 3 seconds. Do not press any keys.");
    std::thread::sleep(Duration::from_secs(3));

    let (path, _) = &keyboards[selected];
    let mut device = Device::open(path).context("Failed to reopen selected device")?;

    println!("Grabbing {}", path.display());
    device.grab().context("Failed to grab device")?;

    let mut vdev = create_virtual_keyboard(&device)?;
    let running = install_ctrlc_handler()?;

    run_event_loop(&mut device, &mut vdev, running)?;

    println!("Releasing keyboard");
    device.ungrab()?;

    Ok(())
}

fn is_likely_keyboard(device: &Device) -> bool {
    device
        .supported_keys()
        .map(|keys| {
            keys.contains(KeyCode::KEY_A)
                && keys.contains(KeyCode::KEY_Z)
                && keys.contains(KeyCode::KEY_SPACE)
        })
        .unwrap_or(false)
}

fn emit_key(vdev: &mut VirtualDevice, key: KeyCode, value: i32) {
    let event = InputEvent::new(EventType::KEY.0, key.code(), value);
    let _ = vdev.emit(&[event]);
}

fn handle_axis_pair(
    vdev: &mut VirtualDevice,
    pressed: bool,
    this_down: &mut bool,
    other_down: bool,
    this_key: KeyCode,
    other_key: KeyCode,
) {
    *this_down = pressed;

    emit_key(vdev, this_key, pressed as i32);

    if other_down {
        emit_key(vdev, other_key, (!pressed) as i32);
    }
}

fn enumerate_keyboards() -> Result<Vec<(PathBuf, String)>> {
    let mut keyboards = Vec::new();

    for entry in evdev::enumerate() {
        let device =
            Device::open(&entry.0).with_context(|| format!("Failed to open {:?}", entry.0))?;

        if is_likely_keyboard(&device) {
            let name = device.name().unwrap_or("Unknown keyboard").to_string();
            keyboards.push((entry.0, name));
        }
    }

    if keyboards.is_empty() {
        anyhow::bail!("No keyboards found. Do you have required permissions?");
    }

    Ok(keyboards)
}

fn select_keyboard(keyboards: &[(PathBuf, String)]) -> Result<usize> {
    for (idx, (path, name)) in keyboards.iter().enumerate() {
        println!("{idx}: {name} ({})", path.display());
    }

    print!("Select keyboard: ");
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;

    Ok(input.trim().parse()?)
}

fn create_virtual_keyboard(device: &Device) -> Result<VirtualDevice> {
    let keys = device
        .supported_keys()
        .context("Failed to query supported keys")?;

    VirtualDevice::builder()?
        .name("snap-tap-virtual")
        .with_keys(keys)?
        .build()
        .context("Failed to create virtual keyboard")
}

fn install_ctrlc_handler() -> Result<Arc<AtomicBool>> {
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();

    ctrlc::set_handler(move || {
        flag.store(false, Ordering::SeqCst);
    })?;

    Ok(running)
}

fn run_event_loop(
    device: &mut Device,
    vdev: &mut VirtualDevice,
    running: Arc<AtomicBool>,
) -> Result<()> {
    let mut a_down = false;
    let mut d_down = false;
    let mut w_down = false;
    let mut s_down = false;

    let raw_fd = device.as_raw_fd();

    let borrowed_fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
    let mut poll_fds = [PollFd::new(borrowed_fd, PollFlags::POLLIN)];

    while running.load(Ordering::SeqCst) {
        let ready = match poll(&mut poll_fds, 50u16) {
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
        };

        if ready == 0 {
            continue;
        }

        if !poll_fds[0]
            .revents()
            .map(|e| e.contains(PollFlags::POLLIN))
            .unwrap_or(false)
        {
            continue;
        }

        for event in device.fetch_events()? {
            if event.event_type() != EventType::KEY {
                let _ = vdev.emit(&[event]);
                continue;
            }

            let event_value = event.value();

            if event_value == 2 {
                continue;
            }

            let pressed = event.value() == 1;

            match event.code() {
                code if code == KeyCode::KEY_A.code() => {
                    handle_axis_pair(
                        vdev,
                        pressed,
                        &mut a_down,
                        d_down,
                        KeyCode::KEY_A,
                        KeyCode::KEY_D,
                    );
                }
                code if code == KeyCode::KEY_D.code() => {
                    handle_axis_pair(
                        vdev,
                        pressed,
                        &mut d_down,
                        a_down,
                        KeyCode::KEY_D,
                        KeyCode::KEY_A,
                    );
                }
                code if code == KeyCode::KEY_W.code() => {
                    handle_axis_pair(
                        vdev,
                        pressed,
                        &mut w_down,
                        s_down,
                        KeyCode::KEY_W,
                        KeyCode::KEY_S,
                    );
                }
                code if code == KeyCode::KEY_S.code() => {
                    handle_axis_pair(
                        vdev,
                        pressed,
                        &mut s_down,
                        w_down,
                        KeyCode::KEY_S,
                        KeyCode::KEY_W,
                    );
                }
                _ => {
                    let _ = vdev.emit(&[event]);
                }
            }
        }
    }

    Ok(())
}
