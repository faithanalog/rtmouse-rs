extern crate x11;

use std::cmp::max;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::prelude::*;
use std::io::BufReader;
use std::os::raw::{c_char, c_int, c_uint};
use std::ptr::null;
use std::sync::mpsc;
use std::thread;
use std::thread::sleep;
use std::time::*;
use std::time::{Duration, Instant};
use x11::{xinput2, xlib, xtest};

struct DwellConfig {
    min_movement_pixels: u32,
    dwell_time: u32,
    drag_time: u32,
    drag_enabled: bool,
    sound_enabled: bool,
    write_status_file: bool,
    status_file: &'static str,
}

const TIMER_INTERVAL_MS: u32 = 100;

// Default config, may make mutable later
static CONFIG: DwellConfig = DwellConfig {
    // Minimum movement before a mouse motion activates the dwell timer
    min_movement_pixels: 10,

    // rtmouse will wait this long after mouse movement ends before clicking.
    // default 500ms. you may want to make it longer
    dwell_time: 500 / TIMER_INTERVAL_MS,

    // rtmouse will drag-click if you move the mouse within this timeframe
    // after a click occurs.
    drag_time: 500 / TIMER_INTERVAL_MS,

    // dragging only happens when this is on
    drag_enabled: true,

    // sound plays on click when this is on
    sound_enabled: true,

    // status_file will be modified with enabled/disabled/terminated statuses
    // when this is on
    write_status_file: true,

    status_file: "/tmp/rtmouse-status.txt",
};

struct StateActive {
    active: bool,
    just_became_active: bool,
}

struct StateX11 {
    display: *mut xlib::Display,
    xi_extension_opcode: i32,
}

struct StateIsCursorMoving {
    old_x: i32,
    old_y: i32,
    moving: bool,
}

struct StateMainLoop {
    we_are_dragging_mouse: bool,
    idle_timer: u32,
    st_active: StateActive,
    st_x11: StateX11,
    st_is_click_inhibited: StateIsClickInhibited,
    st_is_cursor_moving: StateIsCursorMoving,
}

fn play_click_sound() {}

// via XI2.h: #define XIMaskLen(event) (((event) >> 3) + 1)
fn XIMaskLen(event: i32) -> i32 {
    (event >> 3) + 1
}

fn initialize_x11_state(st_x11: &mut StateX11) {
    let display = unsafe { xlib::XOpenDisplay(null()) };
    if display.is_null() {
        panic!("Error: Failed to open default display");
    }

    let mut opcode = 0;
    let mut evt = 0;
    let mut err = 0;
    unsafe {
        let ext = CString::new("XInputExtension").unwrap();
        if xlib::XQueryExtension(display, ext.as_ptr(), &mut opcode, &mut evt, &mut err) == 0 {
            panic!("Error: initialize_x11_state: could not query XInputExtension.");
        }
    }

    st_x11.display = display;
    st_x11.xi_extension_opcode = opcode;

    let root = unsafe { xlib::XDefaultRootWindow(display) };

    let mask_len = XIMaskLen(xinput2::XI_LASTEVENT);
    let mut mask_buf = vec![0u8; mask_len as usize];
    let mut m = xinput2::XIEventMask {
        deviceid: xinput2::XIAllDevices,
        mask_len,
        mask: mask_buf.as_mut_ptr(),
    };
    xinput2::XISetMask(&mut mask_buf[..], xinput2::XI_RawButtonPress);
    xinput2::XISetMask(&mut mask_buf[..], xinput2::XI_RawButtonRelease);

    unsafe {
        xinput2::XISelectEvents(display, root, &mut m, 1);
        xlib::XSync(display, 0);
    }
}

struct StateIsClickInhibited {
    inhibit_mask: u64,
    uninhibit_mask: u64,
}

fn is_click_inhibited(st: &mut StateIsClickInhibited, st_x11: &StateX11) -> bool {
    st.inhibit_mask &= !st.uninhibit_mask;
    st.uninhibit_mask = 0;

    let display = st_x11.display;

    unsafe {
        while xlib::XPending(display) > 0 {
            let mut ev = std::mem::MaybeUninit::uninit();
            xlib::XNextEvent(display, ev.as_mut_ptr());
            let ev = ev.assume_init();
            let mut cookie = ev.generic_event_cookie;

            if xlib::XGetEventData(display, &mut cookie) != 0
                && cookie.type_ == xlib::GenericEvent
                && cookie.extension == st_x11.xi_extension_opcode
            {
                let data: *mut xinput2::XIRawEvent = cookie.data.cast();

                match cookie.evtype {
                    xinput2::XI_RawButtonPress => {
                        st.inhibit_mask |= 1 << (*data).detail;
                    }
                    xinput2::XI_RawButtonRelease => {
                        st.uninhibit_mask |= 1 << (*data).detail;
                    }
                    _ => {}
                }
            }
        }
    }

    st.inhibit_mask != 0
}

fn is_cursor_moving(st: &mut StateIsCursorMoving, st_x11: &StateX11) -> bool {
    let display = st_x11.display;

    let mut root_x = 0;
    let mut root_y = 0;
    let mut root_win = unsafe { xlib::XDefaultRootWindow(display) };

    let mut child_x = 0;
    let mut child_y = 0;
    let mut child_win = std::mem::MaybeUninit::uninit();

    let mut button_mask = 0;

    unsafe {
        xlib::XQueryPointer(
            display,
            root_win,
            &mut root_win,
            child_win.as_mut_ptr(),
            &mut root_x,
            &mut root_y,
            &mut child_x,
            &mut child_y,
            &mut button_mask,
        );
    }

    let dx = root_x - st.old_x;
    let dy = root_y - st.old_y;

    let movement_threshold = if st.moving {
        1
    } else {
        CONFIG.min_movement_pixels
    };

    st.moving = (dx * dx + dy * dy) as u32 > movement_threshold * movement_threshold;

    if st.moving {
        st.old_x = root_x;
        st.old_y = root_y;
    }

    st.moving
}

fn get_primary_button_code(st_x11: &StateX11) -> u8 {
    let mut primary_button = 0;
    if unsafe { xlib::XGetPointerMapping(st_x11.display, &mut primary_button, 1) } < 1 {
        primary_button = 1
    }
    primary_button
}

fn send_button_event(st_x11: &StateX11, btn: u8, state: bool, delay: u32) {
    unsafe {
        xtest::XTestFakeButtonEvent(st_x11.display, btn.into(), state.into(), delay.into());
    }
}

fn main_loop(st: &mut StateMainLoop) {
    if !st.st_active.active {
        return;
    }

    let max_time = max(CONFIG.dwell_time, CONFIG.drag_time) + 1;

    if is_cursor_moving(&mut st.st_is_cursor_moving, &st.st_x11) {
        if st.st_active.just_became_active {
            st.st_active.just_became_active = false;
            st.idle_timer = max_time + 1;
        } else {
            st.idle_timer = 0;
        }
        return;
    }

    if st.idle_timer < max_time {
        st.idle_timer += 1;
    }

    if is_click_inhibited(&mut st.st_is_click_inhibited, &st.st_x11) {
        if !CONFIG.drag_enabled || !st.we_are_dragging_mouse {
            st.idle_timer = max_time;
        }
    }

    if st.idle_timer == CONFIG.dwell_time && !st.we_are_dragging_mouse {
        let primary_button = get_primary_button_code(&st.st_x11);
        if CONFIG.drag_enabled {
            send_button_event(&st.st_x11, primary_button, true, 0);

            st.we_are_dragging_mouse = true;
            st.idle_timer = 0;
        } else {
            send_button_event(&st.st_x11, primary_button, true, 0);
            send_button_event(&st.st_x11, primary_button, false, 0);

            st.idle_timer = max_time;
        }
        play_click_sound();
    }

    if st.idle_timer == CONFIG.drag_time && st.we_are_dragging_mouse {
        let primary_button = get_primary_button_code(&st.st_x11);
        send_button_event(&st.st_x11, primary_button, false, 0);

        st.we_are_dragging_mouse = false;
        st.idle_timer = max_time;
    }
}

fn main() {
    let mut st = StateMainLoop {
        idle_timer: 0,
        we_are_dragging_mouse: false,
        st_active: StateActive {
            active: true,
            just_became_active: true,
        },
        st_is_click_inhibited: StateIsClickInhibited {
            inhibit_mask: 0,
            uninhibit_mask: 0,
        },
        st_is_cursor_moving: StateIsCursorMoving {
            old_x: 0,
            old_y: 0,
            moving: false,
        },
        st_x11: StateX11 {
            display: std::ptr::null_mut(),
            xi_extension_opcode: 0,
        },
    };

    initialize_x11_state(&mut st.st_x11);

    let mut next_tick = Instant::now();
    let tick_duration = Duration::from_millis(TIMER_INTERVAL_MS as u64);

    loop {
        main_loop(&mut st);
        let now = Instant::now();
        while next_tick <= now {
            next_tick += tick_duration;
        }
        sleep(next_tick - now);
    }
}
