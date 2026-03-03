use chrono::{DateTime, Datelike, Local};
use clap::{Arg, Command};
use log::LevelFilter;
use midir::{MidiInput, MidiInputConnection, MidiInputPort};
use midly::live::LiveEvent;
use midly::num::u28;
use midly::{Format, Header, MidiMessage, Smf, Timing, Track, TrackEvent, TrackEventKind};
use signal_hook::consts::signal::*;
use signal_hook::flag;
use std::collections::HashSet;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fs, io};

const PACKAGE_NAME: &str = env!("CARGO_PKG_NAME");
const DEFAULT_USEC_PER_TICK: u32 = 500; // 120 BPM with 1000 ticks per beat
const DEFAULT_TICKS_PER_BEAT: u16 = 1000;

struct RecordingSession {
    first_event_time: Option<Instant>,
    last_event_time: Option<Instant>,
    usec_per_tick: u32,
    events: Vec<TrackEvent<'static>>,

    // For pause detection
    active_lanes: HashSet<Lane>,
}

#[derive(Debug, Hash, PartialEq, Eq)]
enum LaneType {
    Note,
    Cc,
}

// Channel, cc/midi, controller/note
#[derive(Debug, Hash, PartialEq, Eq)]
struct Lane(u8, LaneType, u8);

impl Lane {
    // See https://anotherproducer.com/online-tools-for-musicians/midi-cc-list/
    const PEDALS: [u8; 6] = [64, 65, 66, 67, 68, 69];

    fn index(ev: TrackEventKind) -> Option<(bool, Lane)> {
        match ev {
            TrackEventKind::Midi { channel, message } => match message {
                MidiMessage::NoteOn { key, .. } => {
                    Some((true, Lane(channel.as_int(), LaneType::Note, key.as_int())))
                }
                MidiMessage::NoteOff { key, .. } => {
                    Some((false, Lane(channel.as_int(), LaneType::Note, key.as_int())))
                }
                MidiMessage::Controller { controller, value } => {
                    if Self::PEDALS.contains(&controller.as_int()) {
                        Some((
                            value >= 64,
                            Lane(channel.as_int(), LaneType::Cc, controller.as_int()),
                        ))
                    } else {
                        None
                    }
                }
                _ => None,
            },
            _ => None,
        }
    }
}

impl RecordingSession {
    fn new() -> Self {
        RecordingSession {
            first_event_time: None,
            last_event_time: None,
            usec_per_tick: DEFAULT_USEC_PER_TICK,
            events: Vec::new(),
            active_lanes: HashSet::new(),
        }
    }

    fn add_event(&mut self, event: LiveEvent<'static>) {
        let now = Instant::now();
        if self.first_event_time.is_none() {
            self.first_event_time = Some(now);
        }
        let elapsed_since_last = self
            .last_event_time
            .map(|t| now.duration_since(t))
            .unwrap_or(Duration::ZERO);
        let delta_ticks =
            (elapsed_since_last.as_micros() as u64 / self.usec_per_tick as u64) as u32;
        self.last_event_time = Some(now);

        // Convert LiveEvent to TrackEventKind
        if let Some(kind) = Self::live_event_to_track_event_kind(event) {
            self.events.push(TrackEvent {
                delta: u28::from(delta_ticks),
                kind,
            });

            if let Some((on, lane)) = Lane::index(kind) {
                if on {
                    self.active_lanes.insert(lane);
                } else {
                    self.active_lanes.remove(&lane);
                }
                log::debug!("Active lanes {:?}", self.active_lanes)
            }
        }
    }

    fn live_event_to_track_event_kind(
        event: LiveEvent<'static>,
    ) -> Option<TrackEventKind<'static>> {
        match event {
            LiveEvent::Midi { channel, message } => Some(TrackEventKind::Midi { channel, message }),
            LiveEvent::Common(_) => None,
            LiveEvent::Realtime(_) => None,
        }
    }

    fn target_directory(base_path: &PathBuf, time: DateTime<Local>) -> std::io::Result<PathBuf> {
        let directory = Path::new(base_path)
            .join(time.year().to_string())
            .join(time.month().to_string())
            .join(time.day().to_string());

        fs::create_dir_all(&directory)?;

        if !directory.is_dir() {
            return Err(io::Error::new(
                ErrorKind::AlreadyExists,
                format!("Path exists but is not a directory {}", directory.display()),
            ));
        }
        Ok(directory)
    }

    fn save_to_file(&mut self, directory: &PathBuf) -> std::io::Result<()> {
        if self.first_event_time.is_none() {
            assert!(self.events.is_empty());
            log::info!("No more events, skipping save.");
            return Ok(());
        }
        assert!(!self.events.is_empty() && self.last_event_time.is_some());
        let file_time = chrono::Local::now();
        let file_path = Self::target_directory(directory, file_time)?.join(format!(
            "{}-{}e-{}s.mid",
            file_time.format("%FT%H:%M:%S%Z"),
            self.events.len() + 1, // + EndOfTrack
            self.last_event_time
                .unwrap()
                .duration_since(self.first_event_time.unwrap())
                .as_secs_f64()
                .ceil() as i64
        ));

        self.events.push(TrackEvent {
            delta: u28::from(0),
            kind: TrackEventKind::Meta(midly::MetaMessage::EndOfTrack),
        });

        let timing = Timing::Metrical(midly::num::u15::from(DEFAULT_TICKS_PER_BEAT));
        let header = Header::new(Format::SingleTrack, timing);
        let mut smf = Smf::new(header);

        let mut track = Track::new();
        track.extend_from_slice(&self.events);
        smf.tracks.push(track);

        let mut output = Vec::new();
        smf.write(&mut output).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("MIDI write error: {:?}", e),
            )
        })?;

        log::info!("Writing recording to {:}", &file_path.display());
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true) // Do not overwrite.
            .open(&file_path)?;
        file.write_all(&output)?;
        println!("Wrote {} events.", self.events.len());
        self.reset();

        Ok(())
    }

    fn reset(&mut self) {
        self.first_event_time = None;
        self.last_event_time = None;
        self.events.clear();
    }
}

fn list_midi_inputs() -> Result<(), Box<dyn std::error::Error>> {
    let midi_input = MidiInput::new(PACKAGE_NAME)?;
    let ports = midi_input.ports();

    if ports.is_empty() {
        println!("No MIDI input ports available.");
    } else {
        println!("Available MIDI input ports {}:\n", &ports.len());
        for port in ports {
            let name = midi_input.port_name(&port)?;
            println!("\t{}", name);
        }
    }
    Ok(())
}

fn recording_loop(
    port_name_prefix: &str,
    output_path: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let stop = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&stop))?;

    // Only stop on clean exit. This helps to survive temporary conditions like controller
    // not yet connected or temporarily disconnected.
    let retry_delay = Duration::from_secs(4);
    while let Err(err) = do_recording(stop.clone(), port_name_prefix, output_path.to_owned()) {
        log::error!("Error: {}", err);
        if stop.load(Ordering::Relaxed) {
            println!();
            break;
        }
        log::info!(
            "Waiting for {} seconds before retry.",
            retry_delay.as_secs()
        );
        std::thread::sleep(retry_delay);
    }
    Ok(())
}

fn do_recording(
    stop: Arc<AtomicBool>,
    port_name_prefix: &str,
    output_path: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let session = Arc::new(Mutex::new(RecordingSession::new()));
    let disconnected = Arc::new(AtomicBool::new(false));
    let mut connection = connect(port_name_prefix, session.clone(), disconnected.to_owned())?;

    log::info!("Recording... Press Ctrl+C to stop.");
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        if let Ok(mut session) = session.try_lock() {
            if let Some(t) = session.last_event_time {
                if Instant::now().duration_since(t) > Duration::from_secs(8)
                    && session.active_lanes.is_empty()
                {
                    session.save_to_file(&output_path)?;
                }
            }
        }
        if disconnected.load(Ordering::Relaxed) {
            connection = connect(port_name_prefix, session.clone(), disconnected.to_owned())?;
        }
    }
    connection.close();
    session.lock().unwrap().save_to_file(&output_path)?;

    log::info!("Bye.");
    Ok(())
}

fn connect(
    port_name_prefix: &str,
    session: Arc<Mutex<RecordingSession>>,
    disconnected: Arc<AtomicBool>,
) -> Result<MidiInputConnection<()>, Box<dyn Error>> {
    let midi_input = MidiInput::new(PACKAGE_NAME)?;

    let selected_port = select_port(&midi_input, port_name_prefix)?;
    let port = selected_port
        .ok_or_else(|| format!("No MIDI input port found matching '{}'", port_name_prefix))?;

    let session_clone = session.clone();
    let connection = midi_input.connect(
        &port,
        PACKAGE_NAME,
        move |timestamp, message, _| {
            /* WARNING: Using patched versio of `midir` to detect controller disconnection
               with alsa backend (see git sub-module).
               Upstream version just silently stops receiving events without any way
               to detect this situation.
               This can be handled in a more straightforward way by using alsa-rs directly,
               but that would require is a lot more work, and we'll lose portability.

               Upon port unsubscription patched midir returns empty data vector here.
             */
            if message.is_empty() {
                disconnected.store(true, Ordering::Relaxed);
                return;
            }
            // Skip active sensing and clock messages
            if message[0] == 0xFE || message[0] == 0xF8 {
                log::debug!("## event {:?}", message);
                return;
            }

            if let Ok(live_event) = LiveEvent::parse(message) {
                let static_event = live_event.to_static();
                log::debug!("@ {}: {:?}", timestamp, static_event);

                let mut session = session_clone.lock().unwrap();
                session.add_event(static_event);
            } else {
                log::debug!("# event {:?}", message);
            }
        },
        (),
    )?;
    Ok(connection)
}

fn select_port(
    midi_input: &MidiInput,
    port_name_prefix: &str,
) -> Result<Option<MidiInputPort>, Box<dyn std::error::Error>> {
    let ports = midi_input.ports();
    for port in &ports {
        let name = midi_input.port_name(port)?;
        if name.starts_with(port_name_prefix.trim()) {
            log::info!("Selected MIDI input port: '{}'", name);
            return Ok(Some(port.clone()));
        }
    }
    Ok(None)
}

fn main() {
    // TODO (refactoring) Replace pringln with log wherever it makes sense.
    env_logger::builder()
        .filter_level(LevelFilter::Trace)
        .init();
    log::debug!("Checking logger works"); // DEBUG

    let matches = Command::new(PACKAGE_NAME)
        .version(env!("CARGO_PKG_VERSION"))
        .author("Petr Gladkikh")
        .about("Continuously records MIDI events from given MIDI sequencer to file archive.")
        .arg(
            Arg::new("list")
                .short('l')
                .long("list")
                .help("List available MIDI input ports.")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("port")
                .short('p')
                .long("port")
                .value_name("PORT_PREFIX")
                .help("MIDI input port name prefix to use.")
                .required_unless_present("list"),
        )
        .arg(
            Arg::new("archive directory")
                .short('o')
                .long("archive-dir")
                .value_name("FILE")
                .help(
                    "Root directory where recorded MIDI files should be stored.\
                          Will be created if it does not exist.",
                )
                .value_parser(clap::value_parser!(PathBuf))
                .required_unless_present("list"),
        )
        .get_matches();

    let result = if matches.get_flag("list") {
        list_midi_inputs()
    } else {
        let port_prefix = matches.get_one::<String>("port").unwrap();
        let output_path = matches
            .get_one::<PathBuf>("archive directory")
            .unwrap()
            .clone();

        recording_loop(port_prefix, output_path)
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
