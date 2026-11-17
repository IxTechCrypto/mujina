//! Hardware buzzer and sound engine for Avalon Nano 3S (Kendryte K230).
//!
//! Controls `/dev/input/event1` via Linux `pwm-beeper` ABI (`EV_SND`/`SND_TONE`).
//! Operates on a dedicated background worker thread to execute musical note sequences,
//! chimes, and celebration anthems without blocking async Axum endpoints or mining loops.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender, TryRecvError},
    Mutex, OnceLock,
};
use std::thread;
use std::time::Duration;

use crate::api_client::types::{SoundNote, SoundState};

const BEEPER_DEV: &str = "/dev/input/event1";
const EV_SYN: u16 = 0x00;
const EV_SND: u16 = 0x12;
const SND_TONE: u16 = 0x02;
/// Global playback slow-down (7/5 = 1.4x longer notes).
const TEMPO_NUM: u32 = 7;
const TEMPO_DEN: u32 = 5;

/// List of built-in melodies available for playback and testing.
pub const AVAILABLE_TUNES: &[&str] = &[
    "high_diff",
    "block_found",
    "jingle_bells",
    "star_spangled",
    "halloween",
    "knight_rider",
    "mario",
    "sonic",
    "street_fighter",
    "star_trek",
    "marines_hymn",
];

enum BuzzerCmd {
    Play {
        name: String,
        notes: Vec<SoundNote>,
    },
    Stop,
    SetMute(bool),
}

static CMD_TX: OnceLock<Sender<BuzzerCmd>> = OnceLock::new();
static CURRENT_PLAYING: Mutex<Option<String>> = Mutex::new(None);
static IS_MUTED: AtomicBool = AtomicBool::new(false);

fn get_sender() -> &'static Sender<BuzzerCmd> {
    CMD_TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        if let Err(e) = thread::Builder::new()
            .name("buzzer_worker".to_string())
            .spawn(move || run_buzzer_worker(rx))
        {
            eprintln!("[buzzer] failed to spawn worker thread: {e}");
        }
        tx
    })
}

fn write_tone_event(file: &mut File, freq_hz: u32) -> std::io::Result<()> {
    let mut event = [0u8; 24];
    event[16..18].copy_from_slice(&EV_SND.to_ne_bytes());
    event[18..20].copy_from_slice(&SND_TONE.to_ne_bytes());
    let val = freq_hz as i32;
    event[20..24].copy_from_slice(&val.to_ne_bytes());
    file.write_all(&event)?;

    let mut syn = [0u8; 24];
    syn[16..18].copy_from_slice(&EV_SYN.to_ne_bytes());
    file.write_all(&syn)?;
    file.flush()
}

fn silence_buzzer(file: &mut File) {
    let _ = write_tone_event(file, 0);
}

fn run_buzzer_worker(rx: Receiver<BuzzerCmd>) {
    let mut beeper = OpenOptions::new().write(true).open(BEEPER_DEV).ok();
    if beeper.is_none() {
        eprintln!("[buzzer] Note: {BEEPER_DEV} not openable (running on non-hardware host or simulation)");
    }

    let mut next_cmd: Option<BuzzerCmd> = None;

    loop {
        let cmd = if let Some(c) = next_cmd.take() {
            c
        } else {
            match rx.recv() {
                Ok(c) => c,
                Err(_) => break, // Channel disconnected
            }
        };

        match cmd {
            BuzzerCmd::SetMute(mute) => {
                IS_MUTED.store(mute, Ordering::Relaxed);
                if mute {
                    if let Some(ref mut b) = beeper {
                        silence_buzzer(b);
                    }
                    *CURRENT_PLAYING.lock().unwrap() = None;
                }
            }
            BuzzerCmd::Stop => {
                if let Some(ref mut b) = beeper {
                    silence_buzzer(b);
                }
                *CURRENT_PLAYING.lock().unwrap() = None;
            }
            BuzzerCmd::Play { name, notes } => {
                if IS_MUTED.load(Ordering::Relaxed) {
                    continue;
                }
                *CURRENT_PLAYING.lock().unwrap() = Some(name.clone());

                let mut interrupted = false;

                for note in notes {
                    // Pre-check for pending interrupt before starting note
                    match rx.try_recv() {
                        Ok(new_cmd) => {
                            next_cmd = Some(new_cmd);
                            interrupted = true;
                            break;
                        }
                        Err(TryRecvError::Empty) => {}
                        Err(TryRecvError::Disconnected) => return,
                    }

                    // Play note tone
                    if let Some(ref mut b) = beeper {
                        let _ = write_tone_event(b, note.freq_hz);
                    }

                    // Sleep in responsive 20ms slices so user stop/switch is instant
                    let mut remaining = note.duration_ms * TEMPO_NUM / TEMPO_DEN;
                    while remaining > 0 {
                        let slice = remaining.min(20);
                        thread::sleep(Duration::from_millis(slice as u64));
                        remaining -= slice;

                        match rx.try_recv() {
                            Ok(new_cmd) => {
                                next_cmd = Some(new_cmd);
                                interrupted = true;
                                break;
                            }
                            Err(TryRecvError::Empty) => {}
                            Err(TryRecvError::Disconnected) => return,
                        }
                    }

                    if interrupted {
                        break;
                    }

                    // 10ms articulation gap between notes
                    if let Some(ref mut b) = beeper {
                        silence_buzzer(b);
                    }
                    thread::sleep(Duration::from_millis(10));
                }

                // If finished normally or interrupted, ensure buzzer is silenced
                if let Some(ref mut b) = beeper {
                    silence_buzzer(b);
                }

                if !interrupted {
                    let mut cur = CURRENT_PLAYING.lock().unwrap();
                    if cur.as_deref() == Some(&name) {
                        *cur = None;
                    }
                }
            }
        }
    }

    if let Some(ref mut b) = beeper {
        silence_buzzer(b);
    }
}

/// Plays a named built-in tune.
pub fn play_tune(name: &str) -> Result<(), &'static str> {
    let notes = get_tune_notes(name).ok_or("unknown tune name")?;
    let _ = get_sender().send(BuzzerCmd::Play {
        name: name.to_string(),
        notes,
    });
    Ok(())
}

/// Plays a custom sequence of notes.
pub fn play_notes(name: Option<String>, notes: Vec<SoundNote>) {
    let tune_name = name.unwrap_or_else(|| "custom".to_string());
    let _ = get_sender().send(BuzzerCmd::Play {
        name: tune_name,
        notes,
    });
}

/// Immediately silences the buzzer and stops playback.
pub fn stop_sound() {
    let _ = get_sender().send(BuzzerCmd::Stop);
}

/// Enables or disables mute.
pub fn set_mute(muted: bool) {
    let _ = get_sender().send(BuzzerCmd::SetMute(muted));
}

/// Returns the current sound status.
pub fn get_sound_state() -> SoundState {
    SoundState {
        playing: CURRENT_PLAYING.lock().unwrap().clone(),
        available_tunes: AVAILABLE_TUNES.iter().map(|s| s.to_string()).collect(),
        muted: IS_MUTED.load(Ordering::Relaxed),
    }
}

/// Returns the note sequence for a named tune.
pub fn get_tune_notes(name: &str) -> Option<Vec<SoundNote>> {
    match name {
        // ── Mining Celebrations ──────────────────────────────────────────
        "high_diff" => Some(vec![
            SoundNote { freq_hz: 523, duration_ms: 70 },  // C5
            SoundNote { freq_hz: 659, duration_ms: 70 },  // E5
            SoundNote { freq_hz: 784, duration_ms: 70 },  // G5
            SoundNote { freq_hz: 1046, duration_ms: 100 }, // C6
            SoundNote { freq_hz: 0, duration_ms: 30 },     // rest
            SoundNote { freq_hz: 1318, duration_ms: 140 }, // E6
            SoundNote { freq_hz: 1568, duration_ms: 240 }, // G6
        ]),

        "block_found" => Some(vec![
            SoundNote { freq_hz: 523, duration_ms: 150 }, // C5
            SoundNote { freq_hz: 0, duration_ms: 20 },
            SoundNote { freq_hz: 523, duration_ms: 150 }, // C5
            SoundNote { freq_hz: 0, duration_ms: 20 },
            SoundNote { freq_hz: 523, duration_ms: 150 }, // C5
            SoundNote { freq_hz: 0, duration_ms: 20 },
            SoundNote { freq_hz: 523, duration_ms: 350 }, // C5 (held)
            SoundNote { freq_hz: 0, duration_ms: 40 },
            SoundNote { freq_hz: 415, duration_ms: 250 }, // G#4
            SoundNote { freq_hz: 0, duration_ms: 30 },
            SoundNote { freq_hz: 466, duration_ms: 250 }, // A#4
            SoundNote { freq_hz: 0, duration_ms: 30 },
            SoundNote { freq_hz: 523, duration_ms: 220 }, // C5
            SoundNote { freq_hz: 0, duration_ms: 30 },
            SoundNote { freq_hz: 466, duration_ms: 150 }, // A#4
            SoundNote { freq_hz: 523, duration_ms: 600 }, // C5 (victory!)
        ]),

        // ── Holiday & Theme Chimes ───────────────────────────────────────
        "jingle_bells" => Some(vec![
            SoundNote { freq_hz: 659, duration_ms: 140 }, // E5
            SoundNote { freq_hz: 0, duration_ms: 30 },
            SoundNote { freq_hz: 659, duration_ms: 140 }, // E5
            SoundNote { freq_hz: 0, duration_ms: 30 },
            SoundNote { freq_hz: 659, duration_ms: 280 }, // E5
            SoundNote { freq_hz: 0, duration_ms: 100 },
            SoundNote { freq_hz: 659, duration_ms: 140 }, // E5
            SoundNote { freq_hz: 0, duration_ms: 30 },
            SoundNote { freq_hz: 659, duration_ms: 140 }, // E5
            SoundNote { freq_hz: 0, duration_ms: 30 },
            SoundNote { freq_hz: 659, duration_ms: 280 }, // E5
            SoundNote { freq_hz: 0, duration_ms: 100 },
            SoundNote { freq_hz: 659, duration_ms: 140 }, // E5
            SoundNote { freq_hz: 784, duration_ms: 140 }, // G5
            SoundNote { freq_hz: 523, duration_ms: 180 }, // C5
            SoundNote { freq_hz: 587, duration_ms: 100 }, // D5
            SoundNote { freq_hz: 659, duration_ms: 380 }, // E5
            SoundNote { freq_hz: 0, duration_ms: 100 },
            SoundNote { freq_hz: 698, duration_ms: 140 }, // F5
            SoundNote { freq_hz: 698, duration_ms: 140 }, // F5
            SoundNote { freq_hz: 698, duration_ms: 200 }, // F5
            SoundNote { freq_hz: 698, duration_ms: 100 }, // F5
            SoundNote { freq_hz: 698, duration_ms: 140 }, // F5
            SoundNote { freq_hz: 659, duration_ms: 140 }, // E5
            SoundNote { freq_hz: 659, duration_ms: 140 }, // E5
            SoundNote { freq_hz: 659, duration_ms: 100 }, // E5
            SoundNote { freq_hz: 659, duration_ms: 100 }, // E5
            SoundNote { freq_hz: 587, duration_ms: 140 }, // D5
            SoundNote { freq_hz: 587, duration_ms: 140 }, // D5
            SoundNote { freq_hz: 659, duration_ms: 140 }, // E5
            SoundNote { freq_hz: 587, duration_ms: 260 }, // D5
            SoundNote { freq_hz: 784, duration_ms: 360 }, // G5
        ]),

        "star_spangled" => Some(vec![
            SoundNote { freq_hz: 392, duration_ms: 220 }, // G4
            SoundNote { freq_hz: 330, duration_ms: 160 }, // E4
            SoundNote { freq_hz: 262, duration_ms: 280 }, // C4
            SoundNote { freq_hz: 330, duration_ms: 280 }, // E4
            SoundNote { freq_hz: 392, duration_ms: 280 }, // G4
            SoundNote { freq_hz: 523, duration_ms: 550 }, // C5
            SoundNote { freq_hz: 0, duration_ms: 80 },
            SoundNote { freq_hz: 659, duration_ms: 220 }, // E5
            SoundNote { freq_hz: 587, duration_ms: 160 }, // D5
            SoundNote { freq_hz: 523, duration_ms: 280 }, // C5
            SoundNote { freq_hz: 330, duration_ms: 280 }, // E4
            SoundNote { freq_hz: 370, duration_ms: 280 }, // F#4
            SoundNote { freq_hz: 392, duration_ms: 550 }, // G4
        ]),

        "halloween" => Some(vec![
            SoundNote { freq_hz: 880, duration_ms: 180 }, // A5
            SoundNote { freq_hz: 784, duration_ms: 140 }, // G5
            SoundNote { freq_hz: 880, duration_ms: 360 }, // A5
            SoundNote { freq_hz: 0, duration_ms: 120 },
            SoundNote { freq_hz: 784, duration_ms: 110 }, // G5
            SoundNote { freq_hz: 698, duration_ms: 110 }, // F5
            SoundNote { freq_hz: 659, duration_ms: 110 }, // E5
            SoundNote { freq_hz: 587, duration_ms: 110 }, // D5
            SoundNote { freq_hz: 554, duration_ms: 240 }, // C#5
            SoundNote { freq_hz: 587, duration_ms: 420 }, // D5
            SoundNote { freq_hz: 0, duration_ms: 150 },
            SoundNote { freq_hz: 440, duration_ms: 180 }, // A4
            SoundNote { freq_hz: 0, duration_ms: 40 },
            SoundNote { freq_hz: 415, duration_ms: 100 }, // G#4
            SoundNote { freq_hz: 440, duration_ms: 100 }, // A4
            SoundNote { freq_hz: 415, duration_ms: 100 }, // G#4
            SoundNote { freq_hz: 440, duration_ms: 100 }, // A4
            SoundNote { freq_hz: 0, duration_ms: 40 },
            SoundNote { freq_hz: 294, duration_ms: 450 }, // D4
            SoundNote { freq_hz: 277, duration_ms: 250 }, // C#4
            SoundNote { freq_hz: 294, duration_ms: 600 }, // D4
        ]),

        "knight_rider" => Some(vec![
            SoundNote { freq_hz: 294, duration_ms: 90 },  // D4
            SoundNote { freq_hz: 349, duration_ms: 90 },  // F4
            SoundNote { freq_hz: 294, duration_ms: 90 },  // D4
            SoundNote { freq_hz: 440, duration_ms: 140 }, // A4
            SoundNote { freq_hz: 392, duration_ms: 90 },  // G4
            SoundNote { freq_hz: 294, duration_ms: 90 },  // D4
            SoundNote { freq_hz: 349, duration_ms: 90 },  // F4
            SoundNote { freq_hz: 294, duration_ms: 90 },  // D4
            SoundNote { freq_hz: 262, duration_ms: 90 },  // C4
            SoundNote { freq_hz: 294, duration_ms: 200 }, // D4
            SoundNote { freq_hz: 0, duration_ms: 40 },
            SoundNote { freq_hz: 294, duration_ms: 90 },  // D4
            SoundNote { freq_hz: 349, duration_ms: 90 },  // F4
            SoundNote { freq_hz: 294, duration_ms: 90 },  // D4
            SoundNote { freq_hz: 440, duration_ms: 140 }, // A4
            SoundNote { freq_hz: 466, duration_ms: 90 },  // Bb4
            SoundNote { freq_hz: 440, duration_ms: 90 },  // A4
            SoundNote { freq_hz: 349, duration_ms: 90 },  // F4
            SoundNote { freq_hz: 294, duration_ms: 260 }, // D4
        ]),

        // ── Retro Video Games ────────────────────────────────────────────
        "mario" => Some(vec![
            SoundNote { freq_hz: 659, duration_ms: 120 }, // E5
            SoundNote { freq_hz: 0, duration_ms: 30 },
            SoundNote { freq_hz: 659, duration_ms: 120 }, // E5
            SoundNote { freq_hz: 0, duration_ms: 120 },
            SoundNote { freq_hz: 659, duration_ms: 120 }, // E5
            SoundNote { freq_hz: 0, duration_ms: 120 },
            SoundNote { freq_hz: 523, duration_ms: 120 }, // C5
            SoundNote { freq_hz: 659, duration_ms: 150 }, // E5
            SoundNote { freq_hz: 0, duration_ms: 90 },
            SoundNote { freq_hz: 784, duration_ms: 240 }, // G5
            SoundNote { freq_hz: 0, duration_ms: 220 },
            SoundNote { freq_hz: 392, duration_ms: 240 }, // G4
            SoundNote { freq_hz: 0, duration_ms: 220 },
            SoundNote { freq_hz: 523, duration_ms: 180 }, // C5
            SoundNote { freq_hz: 0, duration_ms: 90 },
            SoundNote { freq_hz: 392, duration_ms: 140 }, // G4
            SoundNote { freq_hz: 0, duration_ms: 90 },
            SoundNote { freq_hz: 330, duration_ms: 180 }, // E4
            SoundNote { freq_hz: 0, duration_ms: 90 },
            SoundNote { freq_hz: 440, duration_ms: 140 }, // A4
            SoundNote { freq_hz: 494, duration_ms: 140 }, // B4
            SoundNote { freq_hz: 466, duration_ms: 100 }, // Bb4
            SoundNote { freq_hz: 440, duration_ms: 200 }, // A4
            SoundNote { freq_hz: 392, duration_ms: 130 }, // G4
            SoundNote { freq_hz: 659, duration_ms: 130 }, // E5
            SoundNote { freq_hz: 784, duration_ms: 130 }, // G5
            SoundNote { freq_hz: 880, duration_ms: 180 }, // A5
            SoundNote { freq_hz: 698, duration_ms: 120 }, // F5
            SoundNote { freq_hz: 784, duration_ms: 150 }, // G5
            SoundNote { freq_hz: 659, duration_ms: 180 }, // E5
        ]),

        "sonic" => Some(vec![
            // SE-GA! chant
            SoundNote { freq_hz: 659, duration_ms: 300 }, // SE: E5
            SoundNote { freq_hz: 523, duration_ms: 600 }, // GA: C5
            SoundNote { freq_hz: 0,   duration_ms: 350 }, // rest
            // Main Title Screen Fanfare (Masato Nakamura)
            SoundNote { freq_hz: 659, duration_ms: 110 }, // E5
            SoundNote { freq_hz: 0,   duration_ms: 20 },
            SoundNote { freq_hz: 659, duration_ms: 110 }, // E5
            SoundNote { freq_hz: 0,   duration_ms: 20 },
            SoundNote { freq_hz: 554, duration_ms: 110 }, // C#5
            SoundNote { freq_hz: 440, duration_ms: 130 }, // A4
            SoundNote { freq_hz: 0,   duration_ms: 30 },
            SoundNote { freq_hz: 587, duration_ms: 220 }, // D5
            SoundNote { freq_hz: 0,   duration_ms: 60 },
            SoundNote { freq_hz: 587, duration_ms: 350 }, // D5 (held)
            SoundNote { freq_hz: 0,   duration_ms: 140 },
            SoundNote { freq_hz: 659, duration_ms: 110 }, // E5
            SoundNote { freq_hz: 0,   duration_ms: 20 },
            SoundNote { freq_hz: 659, duration_ms: 110 }, // E5
            SoundNote { freq_hz: 0,   duration_ms: 20 },
            SoundNote { freq_hz: 554, duration_ms: 110 }, // C#5
            SoundNote { freq_hz: 440, duration_ms: 130 }, // A4
            SoundNote { freq_hz: 0,   duration_ms: 30 },
            SoundNote { freq_hz: 784, duration_ms: 220 }, // G5
            SoundNote { freq_hz: 0,   duration_ms: 60 },
            SoundNote { freq_hz: 784, duration_ms: 350 }, // G5 (held)
            SoundNote { freq_hz: 0,   duration_ms: 140 },
            // Ending triumphant flourish
            SoundNote { freq_hz: 659, duration_ms: 130 }, // E5
            SoundNote { freq_hz: 880, duration_ms: 130 }, // A5
            SoundNote { freq_hz: 659, duration_ms: 130 }, // E5
            SoundNote { freq_hz: 831, duration_ms: 260 }, // Ab5
            SoundNote { freq_hz: 0,   duration_ms: 30 },
            SoundNote { freq_hz: 880, duration_ms: 500 }, // A5
        ]),

        "street_fighter" => Some(vec![
            // Street Fighter II: Theme of World Warriors / Main Title Theme (Yoko Shimomura)
            // Intro hits
            SoundNote { freq_hz: 294, duration_ms: 90 },  // D4
            SoundNote { freq_hz: 294, duration_ms: 90 },  // D4
            SoundNote { freq_hz: 294, duration_ms: 90 },  // D4
            SoundNote { freq_hz: 294, duration_ms: 140 }, // D4
            SoundNote { freq_hz: 0,   duration_ms: 120 }, // rest
            // Phrase 1
            SoundNote { freq_hz: 392, duration_ms: 180 }, // G4
            SoundNote { freq_hz: 370, duration_ms: 320 }, // F#4
            SoundNote { freq_hz: 392, duration_ms: 320 }, // G4
            SoundNote { freq_hz: 440, duration_ms: 650 }, // A4
            SoundNote { freq_hz: 0,   duration_ms: 60 },
            // Phrase 2
            SoundNote { freq_hz: 392, duration_ms: 110 }, // G4
            SoundNote { freq_hz: 370, duration_ms: 110 }, // F#4
            SoundNote { freq_hz: 330, duration_ms: 650 }, // E4
            SoundNote { freq_hz: 0,   duration_ms: 80 },
            // Phrase 3 - Rise to the summit
            SoundNote { freq_hz: 370, duration_ms: 160 }, // F#4
            SoundNote { freq_hz: 392, duration_ms: 160 }, // G4
            SoundNote { freq_hz: 440, duration_ms: 320 }, // A4
            SoundNote { freq_hz: 587, duration_ms: 320 }, // D5
            SoundNote { freq_hz: 784, duration_ms: 700 }, // G5
            SoundNote { freq_hz: 0,   duration_ms: 80 },
            // Phrase 4
            SoundNote { freq_hz: 698, duration_ms: 110 }, // F5
            SoundNote { freq_hz: 659, duration_ms: 110 }, // E5
            SoundNote { freq_hz: 587, duration_ms: 320 }, // D5
            SoundNote { freq_hz: 659, duration_ms: 180 }, // E5
            SoundNote { freq_hz: 698, duration_ms: 650 }, // F5
            SoundNote { freq_hz: 0,   duration_ms: 80 },
            // Phrase 5
            SoundNote { freq_hz: 659, duration_ms: 110 }, // E5
            SoundNote { freq_hz: 587, duration_ms: 110 }, // D5
            SoundNote { freq_hz: 466, duration_ms: 320 }, // Bb4
            SoundNote { freq_hz: 784, duration_ms: 700 }, // G5
            SoundNote { freq_hz: 0,   duration_ms: 80 },
            // Phrase 6 - Resolution
            SoundNote { freq_hz: 698, duration_ms: 110 }, // F5
            SoundNote { freq_hz: 659, duration_ms: 110 }, // E5
            SoundNote { freq_hz: 523, duration_ms: 350 }, // C5
            SoundNote { freq_hz: 587, duration_ms: 900 }, // D5
        ]),

        "star_trek" => Some(vec![
            // Star Trek: The Next Generation Main Theme (Courage Fanfare & Goldsmith March)
            // ── Alexander Courage Fanfare ("Space, the final frontier...") ──
            SoundNote { freq_hz: 466, duration_ms: 300 },  // Bb4
            SoundNote { freq_hz: 0,   duration_ms: 30 },
            SoundNote { freq_hz: 622, duration_ms: 600 },  // Eb5
            SoundNote { freq_hz: 587, duration_ms: 200 },  // D5
            SoundNote { freq_hz: 466, duration_ms: 160 },  // Bb4
            SoundNote { freq_hz: 392, duration_ms: 160 },  // G4
            SoundNote { freq_hz: 523, duration_ms: 160 },  // C5
            SoundNote { freq_hz: 698, duration_ms: 1400 }, // F5 (held)
            SoundNote { freq_hz: 0,   duration_ms: 350 },  // dramatic pause
            // ── Jerry Goldsmith Main March Theme ──
            // Motif 1
            SoundNote { freq_hz: 349, duration_ms: 500 },  // F4
            SoundNote { freq_hz: 466, duration_ms: 180 },  // Bb4
            SoundNote { freq_hz: 587, duration_ms: 180 },  // D5
            SoundNote { freq_hz: 523, duration_ms: 650 },  // C5
            SoundNote { freq_hz: 0,   duration_ms: 50 },
            // Motif 2
            SoundNote { freq_hz: 415, duration_ms: 180 },  // Ab4
            SoundNote { freq_hz: 784, duration_ms: 180 },  // G5
            SoundNote { freq_hz: 698, duration_ms: 900 },  // F5
            SoundNote { freq_hz: 0,   duration_ms: 60 },
            // Motif 3 - Climbing fanfare
            SoundNote { freq_hz: 415, duration_ms: 160 },  // Ab4
            SoundNote { freq_hz: 622, duration_ms: 160 },  // Eb5
            SoundNote { freq_hz: 698, duration_ms: 160 },  // F5
            SoundNote { freq_hz: 784, duration_ms: 160 },  // G5
            SoundNote { freq_hz: 622, duration_ms: 160 },  // Eb5
            // Descending brass resolution
            SoundNote { freq_hz: 698, duration_ms: 400 },  // F5
            SoundNote { freq_hz: 622, duration_ms: 400 },  // Eb5
            SoundNote { freq_hz: 587, duration_ms: 400 },  // D5
            SoundNote { freq_hz: 466, duration_ms: 200 },  // Bb4
            SoundNote { freq_hz: 523, duration_ms: 1400 }, // C5 (heroic resolution)
        ]),

        "marines_hymn" | "marine_corps_hymn" => Some(vec![
            // The Marines' Hymn ("From the Halls of Montezuma")
            // Traditional / Jacques Offenbach (Geneviève de Brabant)
            // Authentic military march tempo (135 BPM), Eb Major octave (622Hz-1245Hz)
            // Phrase 1: "From the Halls of Montezuma to the shores of Tripoli"
            SoundNote { freq_hz: 622, duration_ms: 220 }, // Eb5 (From)
            SoundNote { freq_hz: 784, duration_ms: 220 }, // G5 (the)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (Halls)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (of)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (Mon-)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (-te-)
            SoundNote { freq_hz: 932, duration_ms: 660 }, // Bb5 (-zu-)
            SoundNote { freq_hz: 1245, duration_ms: 220 }, // Eb6 (-ma,)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (To)
            SoundNote { freq_hz: 784, duration_ms: 220 }, // G5 (the)
            SoundNote { freq_hz: 831, duration_ms: 220 }, // Ab5 (shores)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (of)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (Tri-)
            SoundNote { freq_hz: 831, duration_ms: 220 }, // Ab5 (-po-)
            SoundNote { freq_hz: 698, duration_ms: 660 }, // F5 (-li;)
            SoundNote { freq_hz: 622, duration_ms: 1100 }, // Eb5
            SoundNote { freq_hz: 0,   duration_ms: 150 }, // rest

            // Phrase 2: "We fight our country's battles in the air, on land, and sea"
            SoundNote { freq_hz: 622, duration_ms: 220 }, // Eb5 (We)
            SoundNote { freq_hz: 784, duration_ms: 220 }, // G5 (fight)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (our)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (coun-)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (-try's)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (bat-)
            SoundNote { freq_hz: 932, duration_ms: 660 }, // Bb5 (-tles)
            SoundNote { freq_hz: 1245, duration_ms: 220 }, // Eb6 (in)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (the)
            SoundNote { freq_hz: 784, duration_ms: 220 }, // G5 (air,)
            SoundNote { freq_hz: 831, duration_ms: 220 }, // Ab5 (on)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (land,)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (and)
            SoundNote { freq_hz: 831, duration_ms: 440 }, // Ab5 (sea;)
            SoundNote { freq_hz: 698, duration_ms: 440 }, // F5
            SoundNote { freq_hz: 622, duration_ms: 1100 }, // Eb5
            SoundNote { freq_hz: 0,   duration_ms: 150 }, // rest

            // Phrase 3: "First to fight for right and freedom, and to keep our honor clean"
            SoundNote { freq_hz: 1245, duration_ms: 220 }, // Eb6 (First)
            SoundNote { freq_hz: 1175, duration_ms: 220 }, // D6 (to)
            SoundNote { freq_hz: 1046, duration_ms: 440 }, // C6 (fight)
            SoundNote { freq_hz: 831, duration_ms: 440 },  // Ab5 (for)
            SoundNote { freq_hz: 1046, duration_ms: 440 }, // C6 (right)
            SoundNote { freq_hz: 831, duration_ms: 440 },  // Ab5 (and)
            SoundNote { freq_hz: 932, duration_ms: 660 },  // Bb5 (free-)
            SoundNote { freq_hz: 1046, duration_ms: 220 }, // C6 (-dom,)
            SoundNote { freq_hz: 932, duration_ms: 440 },  // Bb5
            SoundNote { freq_hz: 1245, duration_ms: 220 }, // Eb6 (and)
            SoundNote { freq_hz: 1175, duration_ms: 220 }, // D6 (to)
            SoundNote { freq_hz: 1046, duration_ms: 440 }, // C6 (keep)
            SoundNote { freq_hz: 831, duration_ms: 440 },  // Ab5 (our)
            SoundNote { freq_hz: 1046, duration_ms: 220 }, // C6 (ho-)
            SoundNote { freq_hz: 1245, duration_ms: 660 }, // Eb6 (-nor)
            SoundNote { freq_hz: 932, duration_ms: 1100 }, // Bb5 (clean;)
            SoundNote { freq_hz: 0,   duration_ms: 150 },  // rest

            // Phrase 4: "We are proud to claim the title of United States Marine"
            SoundNote { freq_hz: 622, duration_ms: 220 }, // Eb5 (We)
            SoundNote { freq_hz: 784, duration_ms: 220 }, // G5 (are)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (proud)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (to)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (claim)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (the)
            SoundNote { freq_hz: 932, duration_ms: 660 }, // Bb5 (ti-)
            SoundNote { freq_hz: 1245, duration_ms: 220 }, // Eb6 (-tle)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (of)
            SoundNote { freq_hz: 784, duration_ms: 220 }, // G5 (U-)
            SoundNote { freq_hz: 831, duration_ms: 220 }, // Ab5 (-ni-)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (-ted)
            SoundNote { freq_hz: 932, duration_ms: 440 }, // Bb5 (States)
            SoundNote { freq_hz: 831, duration_ms: 440 }, // Ab5 (Ma-)
            SoundNote { freq_hz: 698, duration_ms: 440 }, // F5 (-rine.)
            SoundNote { freq_hz: 622, duration_ms: 1400 }, // Eb5 (final triumph)
        ]),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_available_tunes_are_defined_and_valid() {
        for &name in AVAILABLE_TUNES {
            let notes = get_tune_notes(name).unwrap_or_else(|| panic!("tune {name} missing"));
            assert!(!notes.is_empty(), "tune {name} had 0 notes");
            for note in notes {
                assert!(note.freq_hz <= 20000, "note frequency too high: {}", note.freq_hz);
                assert!(note.duration_ms > 0 && note.duration_ms <= 5000, "invalid duration: {}", note.duration_ms);
            }
        }
    }

    #[test]
    fn play_tune_validates_name() {
        assert!(play_tune("nonexistent_tune").is_err());
        assert!(play_tune("mario").is_ok());
        stop_sound();
    }

    #[test]
    fn sound_state_reflects_available_tunes() {
        let state = get_sound_state();
        assert_eq!(state.available_tunes.len(), AVAILABLE_TUNES.len());
        for &t in AVAILABLE_TUNES {
            assert!(state.available_tunes.contains(&t.to_string()));
        }
    }
}
