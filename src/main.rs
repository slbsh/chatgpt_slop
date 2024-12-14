use std::sync::LazyLock;
use std::collections::VecDeque;
use std::io::Write;

use rdev::{Event, EventType, Key};
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct Config {
	#[serde(deserialize_with = "api_key")]
	api_key:    Box<str>,
	#[serde(default)]
	#[serde(deserialize_with = "prompt")]
	prompt:     Box<str>,
	audio_file: Box<str>,
	msg_limit:  usize,
	#[serde(deserialize_with = "device")]
	device:     Box<str>,
	backend:    Box<str>,
	#[serde(default)]
	keycode:    Option<u32>,
	global_listen: bool,
}

fn api_key<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Box<str>, D::Error> {
	let s: String = Deserialize::deserialize(de)?;
	Ok(Box::from(format!("Bearer {s}")))
}

fn prompt<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Box<str>, D::Error> {
	let s: String = Deserialize::deserialize(de)?;
	Ok(Box::from(serde_json::json!({"role": "system", "content": escape_json(&s)}).to_string()))
}

#[cfg(unix)]
fn device<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Box<str>, D::Error> {
	let s: String = Deserialize::deserialize(de)?;
	Ok(Box::from(s))
}

#[cfg(windows)]
fn device<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Box<str>, D::Error> {
	let s: String = Deserialize::deserialize(de)?;
	Ok(Box::from(format!("audio={s}")))
}

const CONFIG_PATH: &str = "config.toml";
static CONFIG: LazyLock<Config> = LazyLock::new(||
	toml::from_str(&std::fs::read_to_string(CONFIG_PATH).unwrap()).unwrap_or_else(|e| {
		eprintln!("Error reading config: {e}");
		std::process::exit(1);
	}));

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	if std::env::args().nth(1) == Some(String::from("keytest")) {
		rdev::listen(|e| println!("{e:?}")).unwrap();
		return Ok(());
	}

	let mut messages: VecDeque<String> = VecDeque::new();
	let client = reqwest::Client::new();
	let mut lines = std::io::stdin().lines();

	let mut block: Box<dyn FnMut(&'static str)> = match CONFIG.global_listen {
		true => Box::new(|p| {
			const DEFAULT_KEY: Key = Key::F1;

			println!("{p}");
			let key = match &CONFIG.keycode {
				Some(k) => Key::Unknown(*k),
				None    => DEFAULT_KEY,
			};

			let (tx, rx) = std::sync::mpsc::channel();
			let handle = tokio::spawn(async move {
				rdev::listen(move |e|
					if let Event { event_type: EventType::KeyPress(k), .. } = e 
						{ if k == key { let _ = tx.send(()); } }).unwrap() });
			rx.recv().unwrap();
			handle.abort();
		}),
		false => Box::new(|p| {
			print!("{p}");
			std::io::stdout().flush().unwrap();
			lines.next().unwrap().unwrap();
		}),
	};

	loop {
		block("Press key to start");
		
		let mut cmd = std::process::Command::new("ffmpeg")
			.args(["-y", "-loglevel", "error", "-f", &CONFIG.backend, "-i", &CONFIG.device, &CONFIG.audio_file])
			.stdin(std::process::Stdio::piped())
			.spawn()?;

		block("Recording..");

		cmd.stdin.as_mut().unwrap().write_all(b"q")?; // lol
		cmd.wait().unwrap();

		let resp = if let serde_json::Value::String(s) = 
			check_err(client.post("https://api.openai.com/v1/audio/transcriptions")
				.header("Authorization", &*CONFIG.api_key)
				.multipart(reqwest::multipart::Form::new()
					.file("file", &*CONFIG.audio_file).await?
					.text("model", "whisper-1"))
				.send().await?).await
				.json::<serde_json::Value>().await?
				.get_mut("text").unwrap().take() { s }
			else { panic!("Invalid response") };

		println!("Transcription: {resp}");
		messages.push_back(serde_json::json!({ "role": "user", "content": escape_json(&resp) }).to_string());

		let resp = if let serde_json::Value::String(s) = 
			check_err(client.post("https://api.openai.com/v1/chat/completions")
				.header("Authorization", &*CONFIG.api_key)
				.header("Content-Type", "application/json")
				.body(format!(r#"{{ "model": "gpt-4o", "messages": [{}, {}] }}"#,
					CONFIG.prompt, messages.iter().enumerate().fold(String::with_capacity(100), 
						|acc, (i, s)| if i == messages.len()-1 { acc + s } else { acc + s + "," })))
				.send().await?).await
				.json::<serde_json::Value>().await?
				.get_mut("choices").unwrap().take()
				.get_mut(0).unwrap().take()
				.get_mut("message").unwrap().take()
				.get_mut("content").unwrap().take() { s }
			else { panic!("Invalid response") };

		println!("Response: {resp}");
		if messages.len() >= CONFIG.msg_limit { messages.pop_front(); }

		let resp = check_err(client.post("https://api.openai.com/v1/audio/speech")
			.header("Authorization", &*CONFIG.api_key)
			.header("Content-Type", "application/json")
			.body(serde_json::json!({ "model": "tts-1", "input": resp, "voice": "onyx" }).to_string())
			.send().await?).await
			.bytes().await?;

		std::process::Command::new("mpv")
			.args(["-", "--no-terminal"])
			.stdin(std::process::Stdio::piped())
			.spawn()?.stdin.unwrap()
			.write_all(&resp)?;
	}
}

async fn check_err(thing: reqwest::Response) -> reqwest::Response {
	match thing.error_for_status_ref() {
		Ok(_) => thing,
		Err(e) => panic!("Error: {e}, {}", String::from_utf8_lossy(&thing.bytes().await.unwrap())),
	}
}

fn escape_json(s: &str) -> String {
	s.replace("\\", "\\\\").replace("\"", "\\\"")
		.replace("\n", "\\n").replace("'", "\\'")
}
