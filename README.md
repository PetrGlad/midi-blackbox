# MIDI Black Box

This is a program that continuously records MIDI events from given sequenser device into SMF (.mid) files archive.
It can be useful in studio to avoid losing good ideas or takes, or for musicians who use MIDI instruments and want
to hear back their performances for practice or sharing.

The program records events continuously, and writes file after a pause is detected or program is stopped.
This means you can launch this program and forget about it. But when you need to get a recording of your performance, 
that recording can be found in the archive.

This program only records MIDI events, and does not do any sound processing.
So usual setup would be to launch this program along with your DAW or sound synthesyser software for audition.

The recorded files layout is as follows: 
* Your Archive Root Direcotry 
  * Year 
    * Month 
      * Day
        * datetime-number_of_events-dureation.mid 

Since MIDI files take very litlle space the program does not have any storage limits.

## Usage

`midi-blackbox --help` - should give brief help message.

`midi-blackbox --list` - list available sequencer ports. 


## Build

ALSA wrapper dependency (used for MIDI input)
`apt install libasound2-dev`.

### Cross complilation for RaspberryPi

Building binary for RaspberryPi on a x86 computer:

```shell
cargo install cross --git https://github.com/cross-rs/cross
sudo apt-get install --yes podman-docker
CROSS_CONTAINER_ENGINE_NO_BUILDKIT=1 cross build --release
```

### Example
```
$ midi-blackbox --list
Available MIDI input ports:

	Midi Through:Midi Through Port-0 14:0
	MPK mini 3:MPK mini 3 MIDI 1 20:0
```


## History

Similar "Archive" function that existed in Pianoteq synthesiser when it was launched as a stand-alone program.
This function was removed recently so I have created a replacement.

This Program was initially a part of [emmate](https://github.com/PetrGlad/emmate) project but is now extracted 
here as it is also useful in itself.

The program is named "blackbox" since its behaviur is somewhat similar to flight data recorders (FDR) which 
also sometimes called "black boxes".


