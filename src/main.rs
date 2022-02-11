extern crate regex;
use actix_files as fs;
use actix_web::{web, get, post, App, Error, HttpRequest, HttpServer, HttpResponse, Responder};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Arc};
use std::sync::mpsc::{channel, Sender};
use std::process::{Command,Stdio};
use std::io::{Write, BufRead, BufReader};
use std::collections::HashMap;
use std::thread;
use regex::Regex;
use std::str::FromStr;

#[derive(Debug)]
struct MyData {
	sender: Sender<String>,
	sgf: String,
	kata_analyze: String,
	kata_raw_nn: String,
	play_com: bool
}

type WebMyData= web::Data<Arc<Mutex<MyData>>>;

#[get("/{filename:.+}")]
async fn index(req: HttpRequest) -> Result<fs::NamedFile, Error> {
	let dir_path  = Path::new("public_html");
	let base_path = PathBuf::from(req.match_info().query("filename"));
	Ok(fs::NamedFile::open(dir_path.join(base_path))?)
}

#[get("/")]
async fn http_get_index() -> Result<fs::NamedFile, Error> {
	let dir_path  = Path::new("public_html");
	Ok(fs::NamedFile::open(dir_path.join("goban.html"))?)
}

#[get("/kata-raw-nn")]
async fn http_get_raw_nn(data: WebMyData) -> impl Responder {
	web::Json(data.lock().unwrap().kata_raw_nn.clone())
}

#[get("/kata-analyze")]
async fn http_get_analyze(data: WebMyData) -> impl Responder {
	web::Json(data.lock().unwrap().kata_analyze.clone())
}

#[get("/sgf")]
async fn http_get_sgf(data: WebMyData) -> impl Responder {
	web::Json(data.lock().unwrap().sgf.clone())
}

#[post("/cmd")]
async fn http_post_cmd(data: WebMyData, info: web::Json<String>) -> impl Responder {
	data.lock().unwrap().sender.send(info.into_inner()).unwrap();
	HttpResponse::Ok()
}

type GtpCmdConv  = fn(&String) -> Vec<String>;
type GtpCallback = fn(bool, &str, &mut MyData); //cmd ret result mydata

struct GtpCommands {
	cmd_alias: HashMap<String, GtpCmdConv>,
	cmd_callback: HashMap<String, GtpCallback>
}

impl GtpCommands {
	fn new() -> Self {
		let mut g = GtpCommands { cmd_alias: HashMap::new(), cmd_callback: HashMap::new() };

		g.register_callback("kata-analyze", |_, s, d| {
			if s.len() > 0 {
				d.kata_analyze = s.to_string();
			}
		});
		g.register_callback("showboard", |_, s, _| {
			println!("{}", s);
		});
		g.register_callback("printsgf", |_, s, d| {
			if s.len() > 0 {
				d.sgf = s.to_string();
			}
		});
		g.register_callback("genmove", |f, _, d| {
			if f {
				d.sender.send("printsgf".to_string()).unwrap();
			}
		});
		g.register_callback("play", |f, _, d| {
			if f {
				d.sender.send("printsgf".to_string()).unwrap();
				if d.play_com {
					d.sender.send("genmove W".to_string()).unwrap();
				}
			}
		});
		g.register_callback("kata-raw-nn", |f, s, d| {
			if f {
				d.kata_raw_nn = String::new();
			}
			d.kata_raw_nn += &(s.to_string() + "\n");
		});

		g.cmd_alias.insert("setup".to_string(), |_| {
			Self::str_to_string_vec(&[
				"name",
				"version",
				"clear_board",
				"boardsize 19",
				"komi 0.5",
				"time_settings 0 5 1",
				"place_free_handicap 3",
                "printsgf"])
		});
		g
	}

	fn register_callback(&mut self, cmd: &str, callback: GtpCallback) {
		self.cmd_callback.insert(cmd.to_string(), callback);
	}

	fn gtp_commands(&self, s: &String) -> Vec<String> {
		let cmd = s.split_whitespace()
				   .next()
				   .unwrap()
				   .to_string();

		if let Some(f) = self.cmd_alias.get(&cmd) {
			return f(&s);
		}
		return [s].iter().map (|x| (*x).clone()).collect::<Vec<_>>();
	}
	fn str_to_string_vec(ss: &[&str]) -> Vec<String> {
		ss.iter()
			.map(|x| x.to_string())
			.collect::<Vec<_>>()
	}
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
	let (sender, receiver) = channel();
	let data = Arc::new(Mutex::new(
			MyData { sender: sender,
					 sgf: String::new(),
					 kata_analyze: String::new(),
					 kata_raw_nn: String::new(),
					 play_com: true
			}));

	let x = data.clone();

	let _t = thread::spawn(move || {
		let process = Command::new("./katago")
			.arg("gtp")
			.arg("-model")
			.arg("a.bin.gz")
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.spawn()
			.expect("failed to spawn");

		let mut proc_out = process.stdin.unwrap();
		let     proc_in  = process.stdout.unwrap();
		let gtp1 = Arc::new(GtpCommands::new());
		let gtp2 = gtp1.clone();

		let cmd_hash1: Arc<Mutex<HashMap<u32, String>>> =
						Arc::new(Mutex::new(HashMap::new()));
		let cmd_hash2 = cmd_hash1.clone();

		let write_thread = thread::spawn(move || {
			let mut cmd_id = 0;
			loop {
				let input = receiver.recv().unwrap();
				for cmd in gtp1.gtp_commands(&input) {
					cmd_id += 1;

					let input_cmd = format!("{} {}\n", cmd_id, cmd);
					cmd_hash1.lock().unwrap().insert(cmd_id, cmd.clone());
					proc_out.write_all(input_cmd.as_bytes()).unwrap();
					print!("GTP: {}", input_cmd);
				}
			}
		});

		let read_thread = thread::spawn(move || {
			let mut bufreader = BufReader::new(proc_in);
			let result_regex = Regex::new(r"^([=?])(\d+)\s+(.*)").unwrap();

			let mut exit_process = false;
			let mut callback: Option<GtpCallback> = None;
			let mut first_time = false;

			loop {
				if exit_process {
					break
				}
				let mut line = String::new();
				match bufreader.read_line(&mut line) {
					Ok(n) => {
						if n == 0 {
							exit_process = true
						} else if n > 0 {
							let arg_str;
							if let Some(caps) = result_regex.captures(&line) {
								print!("GTP: {}", line);
								let [ret_str, ret_id, rest] = [1,2,3].map( |x| {
									caps.get(x).unwrap().as_str().to_string()
								});

								callback = None;
								let id = u32::from_str(&ret_id).unwrap();
								if let Some(ccc) = cmd_hash2.lock().unwrap().remove(&id) {
									if ret_str == "=" { // return true
										let cmd = ccc.split_whitespace()
													   .next()
													   .unwrap_or("");
										if let Some(c) = gtp2.cmd_callback.get(cmd) {
											callback = Some(*c);
											first_time = true;
										}
									}
								} else {
									continue;
								}
								arg_str = rest;
							} else {
								arg_str = line.trim_end().to_string();
								first_time = false;
							}

							if let Some(c) = callback {
								c(first_time, &arg_str, &mut x.lock().unwrap());
							}
						}
					},
					Err(_) => { exit_process = true }
				};
			}
		});

		write_thread.join().unwrap();
		read_thread.join().unwrap();
	});

	HttpServer::new(move || App::new()
			.data(data.clone())
			.service(http_get_index)
			.service(http_post_cmd)
			.service(http_get_raw_nn)
			.service(http_get_sgf)
			.service(http_get_analyze)
			.service(index))
		.bind("0.0.0.0:8080")?
		.run()
		.await
}
