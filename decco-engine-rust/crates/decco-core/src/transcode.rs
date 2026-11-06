use crate::config::SEGMENT_DURATION;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::process::{Child, Command};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedSubtitle {
    pub index: usize,
    pub track_index: usize,
    pub codec: String,
    pub language: String,
    pub title: String,
    pub is_forced: bool,
    pub is_default: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProbeResult {
    pub duration: f64,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub embedded_subtitles: Vec<EmbeddedSubtitle>,
}

static HARDWARE_ENCODER: OnceCell<String> = OnceCell::new();

/// Detects the fastest available H.264 encoder (`h264_nvenc`, `h264_qsv`, `h264_amf`, or `libx264`)
pub async fn detect_hardware_encoder() -> &'static str {
    if let Some(enc) = HARDWARE_ENCODER.get() {
        return enc.as_str();
    }

    let detected = match Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .await
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.contains("h264_nvenc") {
                "h264_nvenc".to_string()
            } else if stdout.contains("h264_qsv") {
                "h264_qsv".to_string()
            } else if stdout.contains("h264_amf") {
                "h264_amf".to_string()
            } else {
                "libx264".to_string()
            }
        }
        Err(_) => "libx264".to_string(),
    };

    info!("[FFmpeg] Selected H.264 encoder: {}", detected);
    let _ = HARDWARE_ENCODER.set(detected);
    HARDWARE_ENCODER
        .get()
        .map(|s| s.as_str())
        .unwrap_or("libx264")
}

/// Converts SRT subtitle content into WebVTT format
pub fn srt_to_webvtt(srt_content: &str) -> String {
    let mut vtt = String::from("WEBVTT\n\n");
    let normalized = srt_content.replace("\r\n", "\n").replace('\r', "\n");
    for raw_line in normalized.lines() {
        let line = raw_line.trim();
        if line.contains("-->") && line.contains(',') {
            vtt.push_str(&line.replace(',', "."));
            vtt.push('\n');
        } else {
            vtt.push_str(line);
            vtt.push('\n');
        }
    }
    vtt
}

/// Probes duration, codecs, and embedded subtitle tracks via `ffprobe`
pub async fn probe_media_url(target_url_or_path: &str) -> anyhow::Result<ProbeResult> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-seekable",
            "0",
            "-probesize",
            "5000000",
            "-analyzeduration",
            "5000000",
            target_url_or_path,
        ])
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!("ffprobe exited with status: {}", output.status);
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let mut result = ProbeResult::default();

    if let Some(duration_str) = json
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(|d| d.as_str())
    {
        if let Ok(dur) = duration_str.parse::<f64>() {
            result.duration = dur;
        }
    } else if let Some(dur_num) = json
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(|d| d.as_f64())
    {
        result.duration = dur_num;
    }

    if let Some(streams) = json.get("streams").and_then(|s| s.as_array()) {
        let mut sub_track_idx = 0usize;
        for stream in streams {
            let codec_type = stream
                .get("codec_type")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            let codec_name = stream
                .get("codec_name")
                .and_then(|c| c.as_str())
                .unwrap_or("unknown")
                .to_string();

            match codec_type {
                "video" if result.video_codec.is_none() => {
                    result.video_codec = Some(codec_name);
                }
                "audio" if result.audio_codec.is_none() => {
                    result.audio_codec = Some(codec_name);
                }
                "subtitle" => {
                    let index = stream
                        .get("index")
                        .and_then(|i| i.as_u64())
                        .unwrap_or(sub_track_idx as u64) as usize;
                    let language = stream
                        .get("tags")
                        .and_then(|t| t.get("language"))
                        .and_then(|l| l.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let title = stream
                        .get("tags")
                        .and_then(|t| t.get("title"))
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("Track {}", sub_track_idx + 1));
                    let is_forced = stream
                        .get("disposition")
                        .and_then(|d| d.get("forced"))
                        .and_then(|f| f.as_i64())
                        == Some(1);
                    let is_default = stream
                        .get("disposition")
                        .and_then(|d| d.get("default"))
                        .and_then(|f| f.as_i64())
                        == Some(1);

                    result.embedded_subtitles.push(EmbeddedSubtitle {
                        index,
                        track_index: sub_track_idx,
                        codec: codec_name,
                        language,
                        title,
                        is_forced,
                        is_default,
                    });
                    sub_track_idx += 1;
                }
                _ => {}
            }
        }
    }

    Ok(result)
}

/// Builds a Virtual VOD HLS `.m3u8` playlist for the given duration in seconds
pub fn build_hls_vod_manifest(duration_secs: f64) -> String {
    let effective_duration = if duration_secs > 0.0 {
        duration_secs
    } else {
        7200.0
    };
    let segment_count = (effective_duration / SEGMENT_DURATION as f64).floor() as usize;

    let mut lines = Vec::with_capacity(segment_count * 2 + 8);
    lines.push("#EXTM3U".to_string());
    lines.push("#EXT-X-VERSION:3".to_string());
    lines.push(format!("#EXT-X-TARGETDURATION:{}", SEGMENT_DURATION));
    lines.push("#EXT-X-MEDIA-SEQUENCE:0".to_string());
    lines.push("#EXT-X-PLAYLIST-TYPE:VOD".to_string());

    for i in 0..segment_count {
        lines.push(format!("#EXTINF:{:.1},", SEGMENT_DURATION as f64));
        lines.push(format!("segment-{}.ts", i));
    }

    lines.push("#EXT-X-ENDLIST".to_string());
    lines.join("\n")
}

/// Spawns an FFmpeg process that outputs MPEG-TS (`video/mp2t`) to stdout for segment `seg_index`.
/// Uses `kill_on_drop(true)` so if the HTTP client disconnects, FFmpeg is killed immediately.
pub async fn spawn_hls_segment_transcoder(
    proxy_url: &str,
    seg_index: u64,
    video_codec: Option<&str>,
) -> anyhow::Result<Child> {
    let start_time = seg_index * SEGMENT_DURATION;
    let is_h264 = video_codec == Some("h264");
    let hw_encoder = detect_hardware_encoder().await;

    let transcode_mode = if is_h264 {
        "Direct Remux".to_string()
    } else {
        format!("Transcoding ({})", hw_encoder)
    };

    info!(
        "[HLS] Parsing seg {} ({}s) | Codec: {} | Mode: {}",
        seg_index,
        start_time,
        video_codec.unwrap_or("Unknown"),
        transcode_mode
    );

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner").arg("-loglevel").arg("warning");

    // For segment 0 (start_time == 0), pass -seekable 0 so FFmpeg streams sequentially from byte 0
    // without jumping to the end of 20GB+ MKV files to parse Cues/Indexes before outputting frames.
    if start_time > 0 {
        cmd.arg("-ss").arg(start_time.to_string());
    } else {
        cmd.arg("-seekable").arg("0");
    }

    cmd.arg("-i").arg(proxy_url);
    cmd.args([
        "-t",
        &SEGMENT_DURATION.to_string(),
        "-output_ts_offset",
        &start_time.to_string(),
        "-c:a",
        "aac",
        "-ac",
        "2",
        "-sn",
    ]);

    if is_h264 {
        cmd.args(["-c:v", "copy"]);
    } else {
        match hw_encoder {
            "h264_nvenc" => {
                cmd.args([
                    "-pix_fmt",
                    "yuv420p",
                    "-c:v",
                    "h264_nvenc",
                    "-preset",
                    "p1",
                    "-cq",
                    "23",
                    "-g",
                    "48",
                ]);
            }
            "h264_qsv" => {
                cmd.args([
                    "-pix_fmt",
                    "nv12",
                    "-c:v",
                    "h264_qsv",
                    "-preset",
                    "veryfast",
                    "-global_quality",
                    "23",
                    "-g",
                    "48",
                ]);
            }
            "h264_amf" => {
                cmd.args([
                    "-pix_fmt",
                    "yuv420p",
                    "-c:v",
                    "h264_amf",
                    "-quality",
                    "speed",
                    "-g",
                    "48",
                ]);
            }
            _ => {
                cmd.args([
                    "-pix_fmt",
                    "yuv420p",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-threads",
                    "0",
                    "-crf",
                    "23",
                    "-g",
                    "48",
                ]);
            }
        }
    }

    cmd.args(["-f", "mpegts", "pipe:1"]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.kill_on_drop(true);

    let child = cmd.spawn().map_err(|e| {
        warn!("[HLS-FFmpeg] Failed to spawn ffmpeg: {}", e);
        e
    })?;

    Ok(child)
}

/// Spawns an FFmpeg process to extract embedded subtitle `track_index` as WebVTT (`text/vtt`)
pub async fn spawn_subtitle_extractor(proxy_url: &str, track_index: usize) -> anyhow::Result<Child> {
    info!("[Subtitles] Extracting track {} from {}", track_index, proxy_url);

    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-i",
        proxy_url,
        "-map",
        &format!("0:s:{}", track_index),
        "-c:s",
        "webvtt",
        "-f",
        "webvtt",
        "pipe:1",
    ]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.kill_on_drop(true);

    let child = cmd.spawn()?;
    Ok(child)
}

