use anyhow::{Context, Result};
use evdev::{Device, EventType, InputEvent, uinput::VirtualDevice};
use nix::poll::{PollFd, PollFlags, poll};
use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn is_likely_keyboard(dev: &Device) -> bool {
    dev.supported_keys()
        .map(|keys| {
            keys.contains(evdev::KeyCode::KEY_A)
                && keys.contains(evdev::KeyCode::KEY_Z)
                && keys.contains(evdev::KeyCode::KEY_SPACE)
        })
        .unwrap_or(false)
}

fn main() -> Result<()> {
    // ---------- enumerate keyboards ----------
    let mut keyboards = Vec::new();

    for path in evdev::enumerate() {
        let dev = Device::open(&path.0).with_context(|| format!("Failed to open {:?}", path))?;

        if is_likely_keyboard(&dev) {
            let name = dev.name().unwrap_or("Unknown keyboard").to_string();
            keyboards.push((path, name));
        }
    }

    if keyboards.is_empty() {
        anyhow::bail!("No keyboards found");
    }

    // ---------- user selection ----------
    let items: Vec<String> = keyboards
        .iter()
        .map(|(p, n)| format!("{n} ({})", p.0.display()))
        .collect();

    for (idx, item) in items.iter().enumerate() {
        println!("{}: {}", idx, item);
    }

    let idx = {
        let mut input = String::new();
        print!("Select keyboard: ");
        std::io::stdout().flush()?;
        std::io::stdin().read_line(&mut input)?;
        input.trim().parse::<usize>()?
    };

    println!("Selected {}", idx);

    println!("Waiting 3 seconds. Do not press any keys.");
    std::thread::sleep(std::time::Duration::from_secs(3));

    let ((path, _), _): &((PathBuf, Device), String) = &keyboards[idx];
    let mut dev = Device::open(path)?;
    println!("Grabbing {}", path.display());

    // ---------- grab ----------
    dev.grab()?;

    // ---------- virtual keyboard ----------
    let keys = dev.supported_keys().unwrap();
    let mut vdev = VirtualDevice::builder()?
        .name("snap-tap-virtual")
        .with_keys(keys)?
        .build()?;

    // ---------- clean exit handling ----------
    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        })?;
    }

    // ---------- state ----------
    let mut a_down = false;
    let mut d_down = false;
    let mut w_down = false;
    let mut s_down = false;

    // prepare poll
    let raw_fd = dev.as_raw_fd();
    // SAFETY:
    // - raw_fd comes from a live evdev::Device
    // - dev outlives the poll loop
    // - poll() does not take ownership of the FD
    let borrowed_fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
    let mut poll_fds = [PollFd::new(borrowed_fd, PollFlags::POLLIN)];

    // ---------- event loop ----------
    while running.load(Ordering::SeqCst) {
        // wait up to 50 ms for input
        let ready = match poll(&mut poll_fds, 50u16) {
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue, // Ctrl+C interrupted poll
            Err(e) => return Err(e.into()),
        };

        if ready == 0 {
            // timeout: no events, loop again and re-check running
            continue;
        }

        if let Some(revents) = poll_fds[0].revents() {
            if !revents.contains(PollFlags::POLLIN) {
                continue;
            }

            // safe: read will not block now
            for ev in dev.fetch_events()? {
                if ev.event_type() == EventType::KEY {
                    match ev.code() {
                        code if code == evdev::KeyCode::KEY_A.code() => {
                            if ev.value() == 1 {
                                a_down = true;
                                emit(&mut vdev, evdev::KeyCode::KEY_A, 1);
                                if d_down {
                                    emit(&mut vdev, evdev::KeyCode::KEY_D, 0);
                                }
                            } else if ev.value() == 0 {
                                a_down = false;
                                emit(&mut vdev, evdev::KeyCode::KEY_A, 0);
                                if d_down {
                                    emit(&mut vdev, evdev::KeyCode::KEY_D, 1);
                                }
                            }
                        }
                        code if code == evdev::KeyCode::KEY_D.code() => {
                            if ev.value() == 1 {
                                d_down = true;
                                emit(&mut vdev, evdev::KeyCode::KEY_D, 1);
                                if a_down {
                                    emit(&mut vdev, evdev::KeyCode::KEY_A, 0);
                                }
                            } else if ev.value() == 0 {
                                d_down = false;
                                emit(&mut vdev, evdev::KeyCode::KEY_D, 0);
                                if a_down {
                                    emit(&mut vdev, evdev::KeyCode::KEY_A, 1);
                                }
                            }
                        }
                        code if code == evdev::KeyCode::KEY_W.code() => {
                            if ev.value() == 1 {
                                w_down = true;
                                emit(&mut vdev, evdev::KeyCode::KEY_W, 1);
                                if s_down {
                                    emit(&mut vdev, evdev::KeyCode::KEY_S, 0);
                                }
                            } else if ev.value() == 0 {
                                w_down = false;
                                emit(&mut vdev, evdev::KeyCode::KEY_W, 0);
                                if s_down {
                                    emit(&mut vdev, evdev::KeyCode::KEY_S, 1);
                                }
                            }
                        }
                        code if code == evdev::KeyCode::KEY_S.code() => {
                            if ev.value() == 1 {
                                s_down = true;
                                emit(&mut vdev, evdev::KeyCode::KEY_S, 1);
                                if w_down {
                                    emit(&mut vdev, evdev::KeyCode::KEY_W, 0);
                                }
                            } else if ev.value() == 0 {
                                s_down = false;
                                emit(&mut vdev, evdev::KeyCode::KEY_S, 0);
                                if w_down {
                                    emit(&mut vdev, evdev::KeyCode::KEY_W, 1);
                                }
                            }
                        }
                        _ => {
                            // forward original event
                            let _ = vdev.emit(&[ev]);
                        }
                    }
                }
            }
        }
    }

    println!("Releasing keyboard");
    dev.ungrab()?;
    Ok(())
}

fn emit(vdev: &mut evdev::uinput::VirtualDevice, key: evdev::KeyCode, value: i32) {
    let ev = InputEvent::new(EventType::KEY.0, key.code(), value);
    let _ = vdev.emit(&[ev]);
}
