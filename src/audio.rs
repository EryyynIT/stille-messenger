//! Аудио-мост звонка (S7): устройства (cpal) ↔ PCM-fifo ядра.
//!
//! Ядро читает микрофон из `<dir>/mic` и пишет динамик в `<dir>/spk`
//! (CORE-API §4.6, PROTOCOL §10): PCM s16le моно 16 кГц, кадры 20 мс
//! (640 байт). Ядро ПЕРЕСОЗДАЁТ fifo на старте каждого звонка (unlink +
//! mkfifo + O_RDWR|O_NONBLOCK), поэтому свои концы открываем только
//! когда звонок активен, с коротким ожиданием появления файла. Громкость
//! и AGC — зона клиента по контракту; mute — нули вместо захвата (ядро
//! их режет DTX'ом, линия жива).

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Частота PCM на fifo (PROTOCOL §10).
pub const SAMPLE_RATE: u32 = 16_000;
/// Кадр — 20 мс.
pub const FRAME_SAMPLES: usize = 320;
/// Байт в кадре (s16le моно).
pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;

/// O_NONBLOCK на Linux (тот же 0o4000, что у ядра в audio.zig); fifo —
/// интерфейс ядра, других ОС у него пока нет.
const O_NONBLOCK: i32 = 0o4000;

/// Сколько ждать появления fifo после accept (ядро открывает свой конец
/// в тот же момент, но гонка возможна).
const FIFO_WAIT: Duration = Duration::from_secs(2);
/// Буферы захвата/вывода: 2 с звука, дальше старое выбрасывается —
/// лучше щелчок, чем растущее отставание.
const RING_CAP: usize = (SAMPLE_RATE * 2) as usize;

/// Кольцевой буфер выборок между потоками (cpal-колбэки не блокируются).
struct Ring {
    buf: Mutex<VecDeque<i16>>,
}

impl Ring {
    fn new() -> Self {
        Self {
            buf: Mutex::new(VecDeque::with_capacity(FRAME_SAMPLES * 4)),
        }
    }

    /// Добавить выборки; переполнение сбрасывает самые старые.
    fn push(&self, samples: &[i16]) {
        let mut buf = self.buf.lock().expect("ring не отравлен");
        if buf.len() + samples.len() > RING_CAP {
            let drop = buf.len() + samples.len() - RING_CAP;
            buf.drain(..drop);
        }
        buf.extend(samples.iter().copied());
    }

    /// Снять до out.len выборок; вернуть сколько снято.
    fn drain(&self, out: &mut [i16]) -> usize {
        let mut buf = self.buf.lock().expect("ring не отравлен");
        let n = out.len().min(buf.len());
        for slot in &mut out[..n] {
            *slot = buf.pop_front().expect("n ограничено длиной");
        }
        n
    }
}

/// Линейный ресемплер между частотами устройства и fifo (16 кГц).
/// Качества хватает для голоса; своих крейтов не тянем.
pub struct LinearResampler {
    in_rate: u32,
    out_rate: u32,
    /// Позиция следующего выходного отсчёта во входном потоке: −1 =
    /// между последней выборкой прошлого куска и первой текущего.
    pos: f64,
    /// Последняя выборка прошлого куска — шов между вызовами.
    prev: i16,
}

impl LinearResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            in_rate,
            out_rate,
            pos: 0.0,
            prev: 0,
        }
    }

    /// Ресемплить input, дописав в out. Моно. Позиция pos живёт между
    /// вызовами: отсчёт на стыке кусков интерполируется между последней
    /// выборкой прошлого куска и первой нового — швов не слышно.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if input.is_empty() {
            return;
        }
        if self.in_rate == self.out_rate {
            out.extend_from_slice(input);
            return;
        }
        let step = f64::from(self.in_rate) / f64::from(self.out_rate);
        let len = input.len() as f64;
        // pos ∈ [-1, len): точка pos лежит между выборками floor(pos) и
        // floor(pos)+1; индекс -1 — последняя выборка прошлого куска.
        // Правее len-1 не идём: там нужна первая выборка СЛЕДУЮЩЕГО куска.
        while self.pos < len - 1.0 {
            let i = self.pos.floor() as i64;
            let frac = self.pos - i as f64;
            let a = if i < 0 { self.prev } else { input[i as usize] };
            let b = input[(i + 1) as usize];
            let mixed = f64::from(a) + (f64::from(b) - f64::from(a)) * frac;
            out.push(
                mixed
                    .round()
                    .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16,
            );
            self.pos += step;
        }
        self.pos -= len;
        self.prev = *input.last().expect("вход непустой");
    }
}

/// Почему звука нет — честная строка для UI (TEXT_STYLE §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioGap {
    /// fifo ядра не появились (старое ядро? звонок без звука).
    NoFifo,
    /// Нет входного устройства (микрофон не подключён/запрещён).
    NoInput,
    /// Нет выходного устройства.
    NoOutput,
}

impl AudioGap {
    pub fn user_text(&self) -> &'static str {
        match self {
            Self::NoFifo => "звук недоступен — ядро не открыло аудио",
            Self::NoInput => "микрофон не найден — вас не слышно",
            Self::NoOutput => "динамик не найден — вы не слышите",
        }
    }
}

/// Живой аудио-мост одного звонка. Drop останавливает потоки и потоки
/// cpal, fifo закрываются; ядро при конце звонка пересоздаёт свои.
pub struct CallAudio {
    stop: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    _input: Option<cpal::Stream>,
    _output: Option<cpal::Stream>,
    /// Чего не хватило для полного звука (None — всё работает).
    gaps: Vec<AudioGap>,
}

impl CallAudio {
    /// Поднять мост: fifo из pcm_dir (~/.stille-messenger/pcm у демона) + устройства
    /// по умолчанию. Не падает: чего не хватило — в `gaps`, звонок идёт
    /// без этого куска (молчание = нули, ядро шлёт DTX-keepalive).
    pub fn start(pcm_dir: &Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        let mut gaps = Vec::new();

        let mic = wait_fifo(&pcm_dir.join("mic"), false);
        let spk = wait_fifo(&pcm_dir.join("spk"), true);
        let (mic, spk) = match (mic, spk) {
            (Some(mic), Some(spk)) => (mic, spk),
            _ => {
                // Без fifo моста нет вообще — устройства не открываем.
                return Self {
                    stop,
                    muted,
                    threads,
                    _input: None,
                    _output: None,
                    gaps: vec![AudioGap::NoFifo],
                };
            }
        };

        // Микрофон: cpal input → ресемпл → ring → поток пишет кадры
        // (или нули при mute) в mic-fifo.
        let mic_ring = Arc::new(Ring::new());
        let input = match open_input(mic_ring.clone()) {
            Some(stream) => Some(stream),
            None => {
                gaps.push(AudioGap::NoInput);
                None
            }
        };
        let writer = {
            let ring = mic_ring.clone();
            let stop = stop.clone();
            let muted = muted.clone();
            let mut mic = mic;
            std::thread::spawn(move || {
                let mut frame = [0i16; FRAME_SAMPLES];
                let mut bytes = [0u8; FRAME_BYTES];
                while !stop.load(Ordering::Relaxed) {
                    if muted.load(Ordering::Relaxed) {
                        ring.drain(&mut frame); // захват выбрасываем
                        frame = [0; FRAME_SAMPLES];
                    } else {
                        let got = ring.drain(&mut frame);
                        for slot in &mut frame[got..] {
                            *slot = 0; // недобор — тишина, не мусор
                        }
                    }
                    for (i, s) in frame.iter().enumerate() {
                        bytes[i * 2..i * 2 + 2].copy_from_slice(&s.to_le_bytes());
                    }
                    // fifo открыт O_NONBLOCK: переполнение (ядро умерло)
                    // — кадр выбрасываем, поток не висим.
                    if mic.write_all(&bytes).is_err() {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };
        threads.push(writer);

        // Динамик: поток читает spk-fifo → ring → cpal output (ресемпл
        // на частоту устройства).
        let spk_ring = Arc::new(Ring::new());
        let reader = {
            let ring = spk_ring.clone();
            let stop = stop.clone();
            let mut spk = spk;
            std::thread::spawn(move || {
                let mut bytes = [0u8; FRAME_BYTES];
                let mut frame = [0i16; FRAME_SAMPLES];
                while !stop.load(Ordering::Relaxed) {
                    match spk.read(&mut bytes) {
                        Ok(0) => std::thread::sleep(Duration::from_millis(5)),
                        Ok(n) => {
                            let samples = n / 2;
                            for (i, s) in frame[..samples].iter_mut().enumerate() {
                                *s = i16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
                            }
                            ring.push(&frame[..samples]);
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            })
        };
        threads.push(reader);
        let output = match open_output(spk_ring) {
            Some(stream) => Some(stream),
            None => {
                gaps.push(AudioGap::NoOutput);
                None
            }
        };

        Self {
            stop,
            muted,
            threads,
            _input: input,
            _output: output,
            gaps,
        }
    }

    /// Микрофон выключен: в fifo уходят нули (ядро режет их DTX'ом).
    pub fn set_muted(&self, on: bool) {
        self.muted.store(on, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Чего не хватило для полного звука.
    pub fn gaps(&self) -> &[AudioGap] {
        &self.gaps
    }
}

impl std::fmt::Debug for CallAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Потоки и cpal-стримы в Debug не выводим — нечего там смотреть.
        f.debug_struct("CallAudio")
            .field("muted", &self.is_muted())
            .field("gaps", &self.gaps)
            .finish()
    }
}

impl Drop for CallAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            // Потоки читают флаг каждые ≤10 мс и не блокируются на fifo
            // (O_NONBLOCK) — join не висит.
            let _ = thread.join();
        }
    }
}

/// Открыть свой конец fifo с ожиданием появления (ядро пересоздаёт
/// файлы на старте звонка). mic — на запись, spk — на чтение; оба
/// O_NONBLOCK, чтобы потоки не висли на записи/чтении.
fn wait_fifo(path: &Path, read: bool) -> Option<File> {
    let started = std::time::Instant::now();
    loop {
        let mut options = OpenOptions::new();
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(O_NONBLOCK);
        let attempt = if read {
            options.read(true).open(path)
        } else {
            // O_WRONLY|O_NONBLOCK без читателя — ENXIO: ядро ещё не
            // открыло свой конец, ждём.
            options.write(true).open(path)
        };
        match attempt {
            Ok(file) => return Some(file),
            Err(_) if started.elapsed() < FIFO_WAIT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// Входное устройство → ring (моно i16, ресемпл на 16 кГц). None —
/// устройства нет или оно не открылось (наушники выдернули и т.п.).
fn open_input(ring: Arc<Ring>) -> Option<cpal::Stream> {
    let device = cpal::default_host().default_input_device()?;
    let config = device.default_input_config().ok()?;
    let channels = config.channels() as usize;
    let rate = config.sample_rate();
    let mut resampler = LinearResampler::new(rate, SAMPLE_RATE);
    let stream = match config.sample_format() {
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config.config(),
            move |data: &[i16], _| {
                let mono = downmix(data, channels);
                let mut out = Vec::with_capacity(mono.len() * 2);
                resampler.process(&mono, &mut out);
                ring.push(&out);
            },
            |_| {},
            None,
        ),
        cpal::SampleFormat::F32 => {
            let ring = ring.clone();
            device.build_input_stream(
                &config.config(),
                move |data: &[f32], _| {
                    let mono: Vec<i16> = downmix_f32(data, channels);
                    let mut out = Vec::with_capacity(mono.len() * 2);
                    resampler.process(&mono, &mut out);
                    ring.push(&out);
                },
                |_| {},
                None,
            )
        }
        _ => return None,
    }
    .ok()?;
    stream.play().ok()?;
    Some(stream)
}

/// Выходное устройство ← ring (ресемпл с 16 кГц на частоту устройства).
fn open_output(ring: Arc<Ring>) -> Option<cpal::Stream> {
    let device = cpal::default_host().default_output_device()?;
    let config = device.default_output_config().ok()?;
    let channels = config.channels() as usize;
    let rate = config.sample_rate();
    let mut resampler = LinearResampler::new(SAMPLE_RATE, rate);
    let stream = match config.sample_format() {
        cpal::SampleFormat::I16 => device.build_output_stream(
            &config.config(),
            move |data: &mut [i16], _| {
                fill_output(data, channels, &ring, &mut resampler, rate);
            },
            |_| {},
            None,
        ),
        cpal::SampleFormat::F32 => device.build_output_stream(
            &config.config(),
            move |data: &mut [f32], _| {
                let mut buf = vec![0i16; data.len()];
                fill_output(&mut buf, channels, &ring, &mut resampler, rate);
                for (slot, s) in data.iter_mut().zip(buf) {
                    *slot = f32::from(s) / f32::from(i16::MAX);
                }
            },
            |_| {},
            None,
        ),
        _ => return None,
    }
    .ok()?;
    stream.play().ok()?;
    Some(stream)
}

/// Заполнить выходной буфер устройства из ring (моно → дубль в каналы).
/// Входа берём ровно столько, сколько ресемплеру нужно на frames выхода
/// (+2 на шов кусков): больше — потеряем выборки, меньше — тишина.
fn fill_output(
    data: &mut [i16],
    channels: usize,
    ring: &Ring,
    resampler: &mut LinearResampler,
    out_rate: u32,
) {
    let frames = data.len() / channels.max(1);
    let need = (frames as u64 * u64::from(SAMPLE_RATE) / u64::from(out_rate) + 2) as usize;
    let mut mono = vec![0i16; need];
    let got = ring.drain(&mut mono);
    let mut upsampled = Vec::with_capacity(frames * 2);
    resampler.process(&mono[..got], &mut upsampled);
    for (i, slot) in data.chunks_mut(channels.max(1)).enumerate() {
        let sample = upsampled.get(i).copied().unwrap_or(0);
        for channel in slot {
            *channel = sample;
        }
    }
}

/// Сведение в моно: среднее каналов.
fn downmix(data: &[i16], channels: usize) -> Vec<i16> {
    data.chunks(channels.max(1))
        .map(|frame| {
            let sum: i32 = frame.iter().map(|s| i32::from(*s)).sum();
            (sum / frame.len() as i32) as i16
        })
        .collect()
}

fn downmix_f32(data: &[f32], channels: usize) -> Vec<i16> {
    data.chunks(channels.max(1))
        .map(|frame| {
            let sum: f32 = frame.iter().sum();
            ((sum / frame.len() as f32) * f32::from(i16::MAX)) as i16
        })
        .collect()
}

/// Каталог fifo демона по его $HOME (~/.stille-messenger/pcm, CORE-API §4.6).
pub fn pcm_dir(home: &Path) -> PathBuf {
    home.join(".stille-messenger").join("pcm")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ресемплер: одинаковые частоты — сквозной; 48→16 кГц — втрое меньше.
    #[test]
    fn resampler_rates() {
        let mut same = LinearResampler::new(16_000, 16_000);
        let mut out = Vec::new();
        same.process(&[1, 2, 3, -4], &mut out);
        assert_eq!(out, vec![1, 2, 3, -4]);

        let mut down = LinearResampler::new(48_000, 16_000);
        let input: Vec<i16> = (0..480).map(|i| i as i16).collect();
        let mut out = Vec::new();
        down.process(&input, &mut out);
        assert_eq!(out.len(), 160, "480 @48k → 160 @16k");
        // Нарезка на куски не даёт швов: поток == один вызов.
        let mut whole = LinearResampler::new(48_000, 16_000);
        let mut expect = Vec::new();
        whole.process(&input, &mut expect);
        let mut chunked = LinearResampler::new(48_000, 16_000);
        let mut got = Vec::new();
        for chunk in input.chunks(100) {
            chunked.process(chunk, &mut got);
        }
        assert_eq!(got, expect, "швы между кусками не слышны");
    }

    /// Кольцо: переполнение сбрасывает старое, drain отдаёт по порядку.
    #[test]
    fn ring_caps_and_drains_in_order() {
        let ring = Ring::new();
        let big: Vec<i16> = (0..RING_CAP as i32).map(|i| i as i16).collect();
        ring.push(&big);
        ring.push(&[7, 8, 9]);
        let mut out = vec![0i16; RING_CAP + 3];
        let n = ring.drain(&mut out);
        assert_eq!(n, RING_CAP, "выше потолка не живёт");
        assert_eq!(&out[n - 3..n], &[7, 8, 9], "хвост — самое свежее");
        assert_eq!(ring.drain(&mut out), 0, "после drain пусто");
    }

    /// Mute по смыслу: нули в кадре — DTX ядра их не шлёт.
    #[test]
    fn muted_frame_is_silence() {
        let frame = [0i16; FRAME_SAMPLES];
        assert!(frame.iter().all(|s| *s == 0));
        assert_eq!(FRAME_BYTES, 640, "20 мс @16k моно s16le");
    }
}
