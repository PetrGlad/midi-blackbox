use chrono::{DateTime, Datelike, Local, TimeZone};
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
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fs, io};

const PACKAGE_NAME: &str = env!("CARGO_PKG_NAME");
const DEFAULT_USEC_PER_TICK: u32 = 500; // 120 BPM with 1000 ticks per beat
const DEFAULT_TICKS_PER_BEAT: u16 = 1000;
const JOURNAL_FILE_NAME: &str = ".recording.journal";
const JOURNAL_MAGIC: &[u8; 5] = b"MBBJ1";
const JOURNAL_HEADER_LEN: usize = JOURNAL_MAGIC.len() + 8;

/// Append-only log of the events of the session that is being recorded. It is a crash-recovery
/// aid only: the session is committed as a proper .mid file and the journal is deleted
/// (see `RecordingSession::save_to_file`), or recovered after an unclean shutdown
/// (see `recover_journal`).
///
/// Format: header (magic, session start as little-endian unix seconds) followed by events,
/// each is a variable-length delta time in ticks and a MIDI channel message with
/// a full status byte (that is, a SMF track body without running status).
struct Journal {
    path: PathBuf,
    writer: BufWriter<File>,
}

impl Journal {
    fn create(directory: &Path, start: DateTime<Local>) -> io::Result<Journal> {
        fs::create_dir_all(directory)?;
        let path = directory.join(JOURNAL_FILE_NAME);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(JOURNAL_MAGIC)?;
        writer.write_all(&start.timestamp().to_le_bytes())?;
        Ok(Journal { path, writer })
    }

    fn append(&mut self, delta: u32, kind: &TrackEventKind<'static>) -> io::Result<()> {
        if let TrackEventKind::Midi { channel, message } = kind {
            let mut buf = Vec::with_capacity(8);
            write_vlq(delta, &mut buf);
            LiveEvent::Midi {
                channel: *channel,
                message: *message,
            }
            .write_std(&mut buf)?;
            self.writer.write_all(&buf)?;
        }
        Ok(())
    }

    /// Make everything appended so far durable.
    fn sync(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
    }

    fn discard(self) -> io::Result<()> {
        let path = self.path.clone();
        drop(self); // Flushes the writer, the file has to be closed before removal on some platforms.
        fs::remove_file(path)
    }
}

fn write_vlq(mut value: u32, out: &mut Vec<u8>) {
    let mut groups = [(value & 0x7f) as u8, 0, 0, 0];
    let mut n = 1;
    value >>= 7;
    while value > 0 {
        groups[n] = (value & 0x7f) as u8 | 0x80;
        value >>= 7;
        n += 1;
    }
    out.extend(groups[..n].iter().rev());
}

/// Returns the value and the number of bytes consumed, `None` if the data is incomplete.
fn read_vlq(data: &[u8]) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    for (i, b) in data.iter().take(4).enumerate() {
        value = (value << 7) | (b & 0x7f) as u32;
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Decodes journal events. Stops at the first incomplete or invalid event
/// (an interrupted write leaves a torn tail).
fn decode_journal_events(mut data: &[u8]) -> Vec<TrackEvent<'static>> {
    let mut events = Vec::new();
    while let Some((delta, n)) = read_vlq(data) {
        let rest = &data[n..];
        let len = match rest.first() {
            Some(0xC0..=0xDF) => 2,
            Some(0x80..=0xEF) => 3,
            _ => break,
        };
        if rest.len() < len {
            break;
        }
        match LiveEvent::parse(&rest[..len]) {
            Ok(LiveEvent::Midi { channel, message }) => events.push(TrackEvent {
                delta: u28::from(delta),
                kind: TrackEventKind::Midi { channel, message },
            }),
            _ => break,
        }
        data = &rest[len..];
    }
    events
}

fn write_smf_file(
    directory: &PathBuf,
    time: DateTime<Local>,
    events: &[TrackEvent<'static>],
    duration_secs: i64,
    suffix: &str,
) -> io::Result<PathBuf> {
    let file_path = RecordingSession::target_directory(directory, time)?.join(format!(
        "{}-{}e-{}s{}.mid",
        time.format("%FT%H:%M:%S%Z"),
        events.len() + 1, // + EndOfTrack
        duration_secs,
        suffix
    ));

    let timing = Timing::Metrical(midly::num::u15::from(DEFAULT_TICKS_PER_BEAT));
    let header = Header::new(Format::SingleTrack, timing);
    let mut smf = Smf::new(header);

    let mut track = Track::new();
    track.extend_from_slice(events);
    track.push(TrackEvent {
        delta: u28::from(0),
        kind: TrackEventKind::Meta(midly::MetaMessage::EndOfTrack),
    });
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
    file.sync_all()?;
    Ok(file_path)
}

/// Turns the journal left by an interrupted session (if any) into a proper .mid file.
fn recover_journal(directory: &PathBuf) -> io::Result<()> {
    let path = directory.join(JOURNAL_FILE_NAME);
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let start = (data.len() >= JOURNAL_HEADER_LEN && data.starts_with(JOURNAL_MAGIC))
        .then(|| {
            i64::from_le_bytes(
                data[JOURNAL_MAGIC.len()..JOURNAL_HEADER_LEN]
                    .try_into()
                    .unwrap(),
            )
        })
        .and_then(|secs| Local.timestamp_opt(secs, 0).single());
    let Some(start) = start else {
        let corrupt = path.with_extension("journal.corrupt");
        log::warn!("Unrecognized journal, moving it to {}", corrupt.display());
        return fs::rename(&path, corrupt);
    };

    let events = decode_journal_events(&data[JOURNAL_HEADER_LEN..]);
    if events.is_empty() {
        log::info!("Found empty journal, removing it.");
        return fs::remove_file(&path);
    }
    let ticks: u64 = events.iter().map(|e| e.delta.as_int() as u64).sum();
    let duration_secs = (ticks * DEFAULT_USEC_PER_TICK as u64).div_ceil(1_000_000) as i64;
    log::warn!(
        "Recovering {} events from the interrupted session started at {}.",
        events.len(),
        start
    );
    write_smf_file(directory, start, &events, duration_secs, "-recovered")?;
    fs::remove_file(&path)
}

struct RecordingSession {
    first_event_time: Option<Instant>,
    last_event_time: Option<Instant>,
    usec_per_tick: u32,
    events: Vec<TrackEvent<'static>>,

    // Crash recovery log of `events`.
    journal_dir: PathBuf,
    journal: Option<Journal>,

    // Is used for pause detection.
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
    fn new(journal_dir: PathBuf) -> Self {
        RecordingSession {
            journal_dir,
            journal: None,
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
            self.journal_event(delta_ticks, &kind);

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

    fn journal_event(&mut self, delta: u32, kind: &TrackEventKind<'static>) {
        // Journal is opened for the first event only, so a failed journal is not retried midway.
        if self.events.len() == 1 {
            match Journal::create(&self.journal_dir, Local::now()) {
                Ok(journal) => self.journal = Some(journal),
                Err(e) => log::warn!("Cannot create journal, session is not crash-safe: {}", e),
            }
        }
        if let Some(journal) = &mut self.journal {
            if let Err(e) = journal.append(delta, kind) {
                log::warn!("Journal write failed, session is not crash-safe: {}", e);
                self.journal = None;
            }
        }
    }

    /// Flush the journal to disk.
    fn sync_journal(&mut self) {
        if let Some(journal) = &mut self.journal {
            if let Err(e) = journal.sync() {
                log::warn!("Journal sync failed: {}", e);
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
            .join(format!("{:02}", time.month()))
            .join(format!("{:02}", time.day()));

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
        let duration_secs = self
            .last_event_time
            .unwrap()
            .duration_since(self.first_event_time.unwrap())
            .as_secs_f64()
            .ceil() as i64;
        write_smf_file(
            directory,
            chrono::Local::now(),
            &self.events,
            duration_secs,
            "",
        )?;
        log::info!("Wrote {} events.", self.events.len());
        self.reset();

        Ok(())
    }

    fn reset(&mut self) {
        self.first_event_time = None;
        self.last_event_time = None;
        self.events.clear();
        if let Some(journal) = self.journal.take() {
            // Only called after the recording is safely in the archive.
            if let Err(e) = journal.discard() {
                log::warn!("Cannot remove journal: {}", e);
            }
        }
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
    let retry_delay = Duration::from_secs(2);
    while let Err(err) = do_recording(stop.clone(), port_name_prefix, output_path.to_owned()) {
        log::warn!("{}", err);
        if stop.load(Ordering::Relaxed) {
            println!();
            break;
        }
        log::debug!(
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
    if let Err(e) = recover_journal(&output_path) {
        log::warn!("Cannot recover interrupted session: {}", e);
    }
    let session = Arc::new(Mutex::new(RecordingSession::new(output_path.clone())));
    let disconnected = Arc::new(AtomicBool::new(false));
    let mut connection = connect(port_name_prefix, session.clone(), disconnected.to_owned())?;

    log::info!("Recording...");
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        if let Ok(mut session) = session.try_lock() {
            if let Some(t) = session.last_event_time {
                let since_last_event = Instant::now().duration_since(t);
                if (session.active_lanes.is_empty() && since_last_event > Duration::from_secs(5))
                    || since_last_event > Duration::from_secs(60)
                {
                    session.save_to_file(&output_path)?;
                }
            }
            session.sync_journal();
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
              Upstream version just silently stops receiving events (at least on ALSA)
              without any way to detect this situation.
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
    let matches = Command::new(PACKAGE_NAME)
        .version(env!("CARGO_PKG_VERSION"))
        .author("Petr Gladkikh")
        .about("Continuously records MIDI events from given MIDI port to file archive.")
        .arg(
            Arg::new("log-level")
                .long("log-level")
                .value_name("LEVEL")
                .help("Set log level. Default: trace.")
                .value_parser(["trace", "debug", "info", "warn", "error"]),
        )
        .arg(
            Arg::new("log-file")
                .long("log-file")
                .value_name("FILE")
                .help("Write log output to the specified file.")
                .value_parser(clap::value_parser!(PathBuf)),
        )
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
                .help("Name prefix of MIDI input port to read from.")
                .required_unless_present("list"),
        )
        .arg(
            Arg::new("archive directory")
                .short('o')
                .long("archive-dir")
                .value_name("FILE")
                .help(
                    "A directory where recorded MIDI files should be stored.\
                          It will be created if it does not exist.",
                )
                .value_parser(clap::value_parser!(PathBuf))
                .required_unless_present("list"),
        )
        .get_matches();

    let log_level = match matches.get_one::<String>("log-level").map(String::as_str) {
        Some("trace") => LevelFilter::Trace,
        Some("debug") => LevelFilter::Debug,
        Some("info") => LevelFilter::Info,
        Some("warn") => LevelFilter::Warn,
        Some("error") => LevelFilter::Error,
        _ => LevelFilter::Info,
    };
    let mut logger = env_logger::builder();
    logger.filter_level(log_level);
    if let Some(log_file_path) = matches.get_one::<PathBuf>("log-file") {
        if let Some(dir) = log_file_path.parent() {
            if dir.exists() {
                if !dir.is_dir() {
                    eprintln!("Log file location {} is not a directory.", dir.display());
                    exit(2);
                }
            } else {
                eprintln!("Creating log directory '{}'", dir.display());
                fs::create_dir_all(&dir).expect("create log file directory");
            }
        }

        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file_path)
        {
            Ok(file) => {
                eprintln!("Writing log to '{}'", log_file_path.display());
                logger.target(env_logger::Target::Pipe(Box::new(file)));
            }
            Err(err) => {
                eprintln!(
                    "Failed to open log file '{}': {}",
                    log_file_path.display(),
                    err
                );
                std::process::exit(2);
            }
        }
    }
    logger.init();

    let result = if matches.get_flag("list") {
        list_midi_inputs()
    } else {
        let port_prefix = matches.get_one::<String>("port").unwrap();
        let output_path = matches
            .get_one::<PathBuf>("archive directory")
            .unwrap()
            .clone();
        log::info!("Archive directory '{}'", output_path.display());
        recording_loop(port_prefix, output_path)
    };

    if let Err(e) = result {
        log::error!("{}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midly::num::{u4, u7};

    fn note_on(delta: u32, key: u8) -> TrackEvent<'static> {
        TrackEvent {
            delta: u28::from(delta),
            kind: TrackEventKind::Midi {
                channel: u4::from(1),
                message: MidiMessage::NoteOn {
                    key: u7::from(key),
                    vel: u7::from(90),
                },
            },
        }
    }

    fn encode(events: &[TrackEvent<'static>]) -> Vec<u8> {
        let mut out = Vec::new();
        for e in events {
            write_vlq(e.delta.as_int(), &mut out);
            if let TrackEventKind::Midi { channel, message } = e.kind {
                LiveEvent::Midi { channel, message }
                    .write_std(&mut out)
                    .unwrap();
            }
        }
        out
    }

    #[test]
    fn vlq_round_trip() {
        for v in [0, 1, 127, 128, 16383, 16384, 0x0fff_ffff] {
            let mut buf = Vec::new();
            write_vlq(v, &mut buf);
            assert_eq!(read_vlq(&buf), Some((v, buf.len())));
        }
    }

    #[test]
    fn journal_events_round_trip() {
        let events = vec![note_on(0, 60), note_on(300, 61), note_on(70000, 62)];
        assert_eq!(decode_journal_events(&encode(&events)), events);
    }

    #[test]
    fn torn_tail_is_dropped() {
        let events = vec![note_on(0, 60), note_on(300, 61)];
        let data = encode(&events);
        for cut in 1..=3 {
            let decoded = decode_journal_events(&data[..data.len() - cut]);
            assert_eq!(decoded, events[..1]);
        }
    }

    #[test]
    fn recover_writes_midi_file_and_removes_journal() {
        let dir = std::env::temp_dir().join(format!("mbb-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let events = vec![note_on(0, 60), note_on(300, 61)];
        let mut data = JOURNAL_MAGIC.to_vec();
        data.extend(Local::now().timestamp().to_le_bytes());
        data.extend(encode(&events));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(JOURNAL_FILE_NAME), data).unwrap();

        recover_journal(&dir).unwrap();

        assert!(!dir.join(JOURNAL_FILE_NAME).exists());
        let mut found = Vec::new();
        let mut stack = vec![dir.clone()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p)
                } else {
                    found.push(p)
                }
            }
        }
        assert_eq!(found.len(), 1);
        let bytes = fs::read(&found[0]).unwrap();
        let smf = Smf::parse(&bytes).unwrap();
        assert_eq!(smf.tracks[0].len(), 3); // 2 notes + EndOfTrack
        let _ = fs::remove_dir_all(&dir);
    }
}
