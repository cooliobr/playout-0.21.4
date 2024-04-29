use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

use regex::Regex;
use simplelog::*;

use crate::filter::FilterType::Audio;
use crate::utils::{
    errors::ProcError, is_close, is_remote, loop_image, sec_to_time, seek_and_length, vec_strings,
    JsonPlaylist, Media, OutputMode::Null, PlayerControl, PlayoutConfig, FFMPEG_IGNORE_ERRORS,
    IMAGE_FORMAT,
};

/// Validate a single media file.
///
/// - Check if file exists
/// - Check if ffmpeg can read the file
/// - Check if Metadata exists
/// - Check if the file is not silent
fn check_media(
    mut node: Media,
    pos: usize,
    begin: f64,
    config: &PlayoutConfig,
) -> Result<(), ProcError> {
    let mut dec_cmd = vec_strings!["-hide_banner", "-nostats", "-threads", "2", "-thread_queue_size", "128", "-hwaccel_device", "0", "-hwaccel", "cuvid", "-hwaccel_output_format", "cuda", "-v", "level+info"];
    let mut error_list = vec![];
    let mut config = config.clone();
    config.out.mode = Null;

    let mut process_length = 0.1;

    if let Some(decoder_input_cmd) = config
        .advanced
        .as_ref()
        .and_then(|a| a.decoder.input_cmd.clone())
    {
        dec_cmd.append(&mut decoder_input_cmd.clone());
    }

    if config.logging.detect_silence {
        process_length = 15.0;
        let seek = node.duration / 4.0;

        // Seek in file, to prevent false silence detection on intros without sound.
        dec_cmd.append(&mut vec_strings!["-ss", seek]);
    }

    // Take care, that no seek and length command is added.
    node.seek = 0.0;
    node.out = node.duration;

    if node
        .source
        .rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .filter(|c| IMAGE_FORMAT.contains(&c.as_str()))
        .is_some()
    {
        node.cmd = Some(loop_image(&node));
    } else {
        node.cmd = Some(seek_and_length(&mut node));
    }

    node.add_filter(&config, &None);

    let mut filter = node.filter.unwrap_or_default();

    if filter.cmd().len() > 1 {
        let re_clean = Regex::new(r"volume=[0-9.]+")?;

        filter.audio_chain = re_clean
            .replace_all(&filter.audio_chain, "anull")
            .to_string();
    }

    filter.add_filter("silencedetect=n=-30dB", 0, Audio);

    dec_cmd.append(&mut node.cmd.unwrap_or_default());
    dec_cmd.append(&mut filter.cmd());
    dec_cmd.append(&mut filter.map());
    dec_cmd.append(&mut vec_strings!["-t", process_length, "-f", "null", "-"]);

    let mut enc_proc = Command::new("ffmpeg")
        .args(dec_cmd)
        .stderr(Stdio::piped())
        .spawn()?;

    let enc_err = BufReader::new(enc_proc.stderr.take().unwrap());
    let mut silence_start = 0.0;
    let mut silence_end = 0.0;
    let re_start = Regex::new(r"silence_start: ([0-9]+:)?([0-9.]+)")?;
    let re_end = Regex::new(r"silence_end: ([0-9]+:)?([0-9.]+)")?;

    for line in enc_err.lines() {
        let line = line?;

        if !FFMPEG_IGNORE_ERRORS.iter().any(|i| line.contains(*i))
            && !config.logging.ignore_lines.iter().any(|i| line.contains(i))
            && (line.contains("[error]") || line.contains("[fatal]"))
        {
            let log_line = line.replace("[error] ", "").replace("[fatal] ", "");

            if !error_list.contains(&log_line) {
                error_list.push(log_line);
            }
        }

        if config.logging.detect_silence {
            if let Some(start) = re_start.captures(&line).and_then(|c| c.get(2)) {
                silence_start = start.as_str().parse::<f32>().unwrap_or_default();
            }

            if let Some(end) = re_end.captures(&line).and_then(|c| c.get(2)) {
                silence_end = end.as_str().parse::<f32>().unwrap_or_default() + 0.5;
            }
        }
    }

    if silence_end - silence_start > process_length {
        error_list.push("Audio is totally silent!".to_string());
    }

    if !error_list.is_empty() {
        error!(
            "<bright black>[Validator]</> ffmpeg error on position <yellow>{pos}</> - {}: <b><magenta>{}</></b>: {}",
            sec_to_time(begin),
            node.source,
            error_list.join("\n")
        )
    }

    error_list.clear();

    if let Err(e) = enc_proc.wait() {
        error!("Validation process: {e:?}");
    }

    Ok(())
}

/// Validate a given playlist, to check if:
///
/// - the source files are existing
/// - file can be read by ffprobe and metadata exists
/// - total playtime fits target length from config
///
/// This function we run in a thread, to don't block the main function.
pub fn validate_playlist(
    mut config: PlayoutConfig,
    player_control: PlayerControl,
    mut playlist: JsonPlaylist,
    is_terminated: Arc<AtomicBool>,
) {
    let date = playlist.date;

    if config.text.add_text && !config.text.text_from_filename {
        // Turn of drawtext filter with zmq, because its port is needed by the decoder instance.
        config.text.add_text = false;
    }

    let mut length = config.playlist.length_sec.unwrap();
    let mut begin = config.playlist.start_sec.unwrap();

    length += begin;

    debug!("Validate playlist from: <yellow>{date}</>");
    let timer = Instant::now();

    for (index, item) in playlist.program.iter_mut().enumerate() {
        if is_terminated.load(Ordering::SeqCst) {
            return;
        }

        let pos = index + 1;

        if !is_remote(&item.source) {
            if item.audio.is_empty() {
                if let Err(e) = item.add_probe(false) {
                    error!(
                        "[Validation] Error on position <yellow>{pos:0>3}</> <yellow>{}</>: {e}",
                        sec_to_time(begin)
                    );
                }
            } else if let Err(e) = item.add_probe(true) {
                error!(
                    "[Validation] Error on position <yellow>{pos:0>3}</> <yellow>{}</>: {e}",
                    sec_to_time(begin)
                );
            }
        }

        if item.probe.is_some() {
            if let Err(e) = check_media(item.clone(), pos, begin, &config) {
                error!("{e}");
            } else if config.general.validate {
                debug!(
                    "[Validation] Source at <yellow>{}</>, seems fine: <b><magenta>{}</></b>",
                    sec_to_time(begin),
                    item.source
                )
            } else if let Ok(mut list) = player_control.current_list.try_lock() {
                // Filter out same item in current playlist, then add the probe to it.
                // Check also if duration differs with playlist value, log error if so and adjust that value.
                list.iter_mut().filter(|list_item| list_item.source == item.source).for_each(|o| {
                    o.probe = item.probe.clone();

                    if let Some(dur) =
                        item.probe.as_ref().and_then(|f| f.format.duration.clone())
                    {
                        let probe_duration = dur.parse().unwrap_or_default();

                        if !is_close(o.duration, probe_duration, 1.2) {
                            error!(
                                "[Validation] File duration (at: <yellow>{}</>) differs from playlist value. File duration: <yellow>{}</>, playlist value: <yellow>{}</>, source <b><magenta>{}</></b>",
                                sec_to_time(o.begin.unwrap_or_default()), sec_to_time(probe_duration), sec_to_time(o.duration), o.source
                            );

                            o.duration = probe_duration;
                        }
                    }

                    if o.audio == item.audio && item.probe_audio.is_some() {
                        o.probe_audio = item.probe_audio.clone();
                        o.duration_audio = item.duration_audio;
                    }
                });
            }
        }

        begin += item.out - item.seek;
    }

    if !config.playlist.infinit && length > begin + 1.2 {
        error!(
            "[Validation] Playlist from <yellow>{date}</> not long enough, <yellow>{}</> needed!",
            sec_to_time(length - begin),
        );
    }

    if config.general.validate {
        info!(
            "[Validation] Playlist length: <yellow>{}</>",
            sec_to_time(begin - config.playlist.start_sec.unwrap())
        );
    }

    debug!(
        "Validation done, in <yellow>{:.3?}</>, playlist length: <yellow>{}</> ...",
        timer.elapsed(),
        sec_to_time(begin - config.playlist.start_sec.unwrap())
    );
}
