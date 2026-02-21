use chrono::{DateTime, Datelike, Local};
use clap::{Arg, Command};
use midir::{MidiInput, MidiInputPort};
use midly::live::LiveEvent;
use midly::num::u28;
use midly::{Format, Header, Smf, Timing, Track, TrackEvent, TrackEventKind};
use signal_hook::consts::signal::*;
use signal_hook::flag;
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
}

impl RecordingSession {
    fn new() -> Self {
        RecordingSession {
            first_event_time: None,
            last_event_time: None,
            usec_per_tick: DEFAULT_USEC_PER_TICK,
            events: Vec::new(),
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
        }
    }

    fn live_event_to_track_event_kind(
        event: LiveEvent<'static>,
    ) -> Option<TrackEventKind<'static>> {
        match event {
            LiveEvent::Midi { channel, message } => Some(TrackEventKind::Midi { channel, message }),
            LiveEvent::Common(_) => None, // Skip common events for now
            LiveEvent::Realtime(_) => None, // Skip realtime events
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
            println!("\nNo events, skipping save.");
            return Ok(());
        }
        assert!(!self.events.is_empty() && self.last_event_time.is_some());
        let file_time = chrono::Local::now();
        let file_path = Self::target_directory(directory, file_time)?.join(format!(
            "{}-{}e-{}s.mid",
            file_time.format("%Y-%m-%d_%H:%M:%S"),
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

        println!("\nWriting recording to {:}", &file_path.display());
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
        println!("Available MIDI input ports:\n");
        for port in ports {
            let name = midi_input.port_name(&port)?;
            println!("\t{}", name);
        }
    }
    Ok(())
}

fn do_recording(
    port_name_prefix: &str,
    output_path: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let midi_input = MidiInput::new(PACKAGE_NAME)?;

    let selected_port = select_port(port_name_prefix, &midi_input)?;
    let port = selected_port
        .ok_or_else(|| format!("No MIDI input port found matching '{}'", port_name_prefix))?;

    let session = Arc::new(Mutex::new(RecordingSession::new()));
    let session_clone = session.clone();

    println!("Recording...");
    println!("Press Ctrl+C to stop.\n");

    let _connection = midi_input.connect(
        &port,
        PACKAGE_NAME,
        move |timestamp, message, _| {
            // Skip active sensing and clock messages
            if message[0] == 0xFE || message[0] == 0xF8 {
                return;
            }

            if let Ok(live_event) = LiveEvent::parse(message) {
                let static_event = live_event.to_static();
                println!("@ {}: {:?}", timestamp, static_event);

                let mut session = session_clone.lock().unwrap();
                session.add_event(static_event);
            }
        },
        (),
    )?;

    let stop = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&stop))?;

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        if let Ok(mut session) = session.try_lock() {
            if let Some(t) = session.last_event_time {
                if Instant::now().duration_since(t) > Duration::from_secs(8) {
                    session.save_to_file(&output_path)?;
                }
            }
        }
    }

    session.lock().unwrap().save_to_file(&output_path)?;

    println!("Bye.");
    Ok(())
}

fn select_port(
    port_name_prefix: &str,
    midi_input: &MidiInput,
) -> Result<Option<MidiInputPort>, Box<dyn Error>> {
    let ports = midi_input.ports();
    for port in &ports {
        let name = midi_input.port_name(port)?;
        if name.starts_with(port_name_prefix.trim()) {
            println!("Selected MIDI input: '{}'", name);
            return Ok(Some(port.clone()));
        }
    }
    Ok(None)
}

fn main() {
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

        do_recording(port_prefix, output_path)
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
