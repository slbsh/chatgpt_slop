use std::sync::LazyLock;
use std::collections::VecDeque;
use std::io::Write;

use rdev::{Event, EventType, Key};

#[derive(serde::Deserialize, Default)]
struct Config {
	api_key:    Box<str>,
	#[serde(default)]
	prompt:     Box<str>,
	audio_file: Box<str>,
	msg_limit:  usize,
	#[cfg(windows)]
	device:     Box<str>,
	#[serde(default)]
	keycode: Option<u32>,
	global_listen: bool,
}

const CONFIG_PATH: &str = "config.toml";
static CONFIG: LazyLock<Config> = LazyLock::new(||
	toml::from_str(&std::fs::read_to_string(CONFIG_PATH).unwrap()).unwrap());

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	if std::env::args().nth(1) == Some(String::from("keytest")) {
		rdev::listen(|e| println!("{:?}", e)).unwrap();
		return Ok(());
	}

	let auth_header = format!("Bearer {}", &*CONFIG.api_key);

	#[cfg(unix)]
	const BACKEND: &str = "alsa";
	#[cfg(unix)]
	const DEVICE: &str = "default";
	#[cfg(windows)]
	const BACKEND: &str = "dshow";
	#[cfg(windows)]
	static DEVICE: LazyLock<String> = LazyLock::new(|| format!("audio={}", &*CONFIG.device));

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
			.args(["-y", "-loglevel", "error", "-f", BACKEND, "-i", &DEVICE, &CONFIG.audio_file])
			.stdin(std::process::Stdio::piped())
			.spawn()?;

		block("Recording..");

		cmd.stdin.as_mut().unwrap().write_all(b"q")?; // lol
		cmd.wait().unwrap();

		let resp = if let serde_json::Value::String(s) = 
			check_err(client.post("https://api.openai.com/v1/audio/transcriptions")
				.header("Authorization", &*auth_header)
				.multipart(reqwest::multipart::Form::new()
					.file("file", &*CONFIG.audio_file).await?
					.text("model", "whisper-1"))
				.send().await?).await
				.json::<serde_json::Value>().await?
				.get_mut("text").unwrap().take() { s }
			else { panic!("Invalid response") };

		println!("Transcription: {resp}");
		messages.push_back(serde_json::json!({ "role": "user", "content": resp }).to_string());

		let resp = if let serde_json::Value::String(s) = 
			check_err(client.post("https://api.openai.com/v1/chat/completions")
				.header("Authorization", &*auth_header)
				.header("Content-Type", "application/json")
				.body(format!(r#"{{ "model": "gpt-4o", "messages": [{{"role": "system", "content": "{}"}}, {}] }}"#,
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
			.header("Authorization", &*auth_header)
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
