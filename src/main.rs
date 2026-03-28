// postal - An opinionated and direct HTTP POST request utility configurable from the command line.
// Copyright (C) 2026  OC
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use {
    axum::{
        extract::{ DefaultBodyLimit, Form, Multipart, },
        http::{ HeaderMap, HeaderName, status::StatusCode, Uri, },
        Router,
        routing::post,
        serve,
    },
    std::{
        collections::HashMap,
        env::args,
        fs::{ create_dir_all, File, },
        io::Write,
        path::PathBuf,
        process::{ Command, Output, },
        sync::OnceLock,
    },
    tempfile::TempDir,
    tokio::net::TcpListener,
};


#[cfg(target_os= "linux")]
use tokio::net::UnixListener;

const HELP: &str = include_str!("../resources/help.txt");

#[derive(Clone, Debug)]
struct Parameter {
    name: String,
    validator: Option<String>,
}

struct Endpoint {
    max_request_size: usize,
    route: PathBuf,
    command: String,
    parameters: Vec<Parameter>,
}

impl Default for Endpoint {
    fn default() -> Endpoint {
        Self {
            max_request_size: 1028_usize,
            route: PathBuf::new(),
            command: String::new(),
            parameters: Vec::new(),
        }
    }
}

#[derive(Default)]
enum Position {
    #[default]
    Delimiter,
    MaxRequestSize,
    Route,
    Path,
    Parameters
}

impl Position {
    fn next(&mut self) {
        let mut next = match self {
            Self::Delimiter => Self::MaxRequestSize,
            Self::MaxRequestSize => Self::Route,
            Self::Route => Self::Path,
            Self::Path => Self::Parameters,
            Self::Parameters => Self::Parameters,
        };

        std::mem::swap(self, &mut next);
    }
}

impl Endpoint {
    fn split_parameter(s: String) -> Parameter {
        let mut split = s.split("->");
        let name = split.next().expect("Parameter name was null").trim().to_owned();
        let validator = split.next().map(|s| s.trim().to_owned());

        Parameter { name, validator, }
    }

    fn from_string(dkv: String) -> Self {
        let mut max_request_size = String::new();
        let mut route = String::new();
        let mut path = String::new();
        let mut parameter = String::new();
        let mut parameters = Vec::new();

        let mut iter = dkv.chars();
        let mut position = Position::default();
        let dlim = iter.next().unwrap();
        position.next();
        for c in iter {
            if c == dlim {
                if let Position::Parameters = position {
                    parameters.push(Self::split_parameter(parameter));
                    parameter = String::new();
                }

                position.next();

                continue;
            }

            match position {
                Position::Delimiter => panic!("WUT"),
                Position::MaxRequestSize => max_request_size.push(c),
                Position::Route => route.push(c),
                Position::Path => path.push(c),
                Position::Parameters => parameter.push(c),
            }
        }

        if let Position::Parameters = position && !parameters.is_empty() {
            parameters.push(Self::split_parameter(parameter));
        }

        Self {
            max_request_size: max_request_size.parse::<usize>().unwrap(),
            route: route.into(),
            command: path,
            parameters,
        }
    }
}

enum EncodedEndpoint {
    Multipart(Endpoint),
    UrlEncoded(Endpoint),
}

static LETTERS: [char; 26] = [
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o',
    'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z',
];

fn base26(mut dec: usize) -> String {
    let mut out = String::new();

    loop {
        let rem = dec % LETTERS.len();
        dec /= LETTERS.len();

        out = format!("{}{out}", LETTERS[rem]);

        if dec == 0 {
            break;
        }
        else {
            dec -= 1;
        }
    }

    out
}

macro_rules! err_400 {
    ($msg:literal$(,$parm:expr)*$(,)*) => {{
        let fmtd = format!($msg$(,$parm)*);
        eprintln!("postal: {fmtd}");
        (StatusCode::BAD_REQUEST, HeaderMap::new(), "Bad request.\n".to_owned())
    }}
}

macro_rules! err_500 {
    ($msg:literal$(,$parm:expr)*$(,)*) => {{
        let fmtd = format!($msg$(,$parm)*);
        eprintln!("postal: {fmtd}");
        (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), "Internal server error.\n".to_owned())
    }}
}

macro_rules! err_500_res {
    ($result:expr,$msg:literal$(,$parm:expr)*$(,)*) => {{
        match $result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{e:#?}");
                return err_500!($msg$(,$parm)*);
            },
        }
    }}
}

macro_rules! err_500_opt {
    ($option:expr,$msg:literal$(,$parm:expr)*$(,)*) => {{
        match $option {
            Some(o) => o,
            None => return err_500!($msg$(,$parm)*),
        }
    }}
}

fn print_stderr(o: &Output) -> Result<(), ()> {
    let err = match String::from_utf8(o.stderr.to_owned()) {
        Ok(err) => err,
        Err(e) => {
            eprintln!("Stderr was not valid UTF-8: {e:?}");
            return Err(());
        },
    };

    let err = err.trim();

    if !err.is_empty() {
        eprintln!("{err}");
    }

    Ok(())
}

fn status_headers_and_content(stdout: Vec<u8>) -> (StatusCode, HeaderMap, String) {
    let output = err_500_res!(String::from_utf8(stdout), "Output of command was not valid UTF-8");

    let mut lines = output.lines();

    let status_str = err_500_opt!(lines.next(), "Command returned no output");
    let status_u16 = err_500_res!(status_str.parse::<u16>(), "'{status_str}' was not a valid status code");
    let status = err_500_res!(StatusCode::from_u16(status_u16), "'{status_str}' was not a known status code");

    let mut headers = HeaderMap::new();
    while let Some(line) = lines.next() && !line.is_empty() {
        let mut kv = line.split(":");

        let key = err_500_opt!(kv.next(), "HTTP header was not in a valid format (K:V)");
        let value = kv.collect::<Vec<&str>>().join(":").trim().to_owned();

        headers.insert(err_500_res!(HeaderName::try_from(key), "Invalid header name '{key}'"), err_500_res!(value.parse(), "Invalid header value '{key}': '{value}'"));
    }

    let mut body = lines.collect::<Vec<&str>>().join("\n");
    body.push('\n');

    (status, headers, body)
}

static NAME: OnceLock<String> = OnceLock::new();

fn main() {
    let mut long_args = args();
    long_args.next(); // burn prog name

    let mut ip = String::from("localhost");
    let mut port = String::from("8080");

    #[cfg(target_os = "linux")]
    let mut unix_socket = None;

    let mut workers = 0_usize;

    let mut endpoints = Vec::new();

    while let Some(long_arg) = long_args.next() {
        if let Some(long_arg) = long_arg.strip_prefix("--") {
            match long_arg {
                "help" => {
                    println!("{HELP}");
                    return;
                },
                "ip" => ip = long_args.next().unwrap(),
                "multipart" => endpoints.push(EncodedEndpoint::Multipart(Endpoint::from_string(long_args.next().unwrap()))),
                "name" => {
                    NAME.get_or_init(|| long_args.next().unwrap());
                },
                "port" => port = long_args.next().unwrap(),
                #[cfg(target_os= "linux")]
                "unix-socket-file" => unix_socket = Some(long_args.next().unwrap()),
                "urlencoded" => endpoints.push(EncodedEndpoint::UrlEncoded(Endpoint::from_string(long_args.next().unwrap()))),
                "workers" => workers = long_args.next().unwrap().parse::<usize>().unwrap(),
                unknown => panic!("Unknown argument --{unknown}"),
            }
        }
        else if let Some(short_args) = long_arg.strip_prefix('-') {
            let mut short_args = short_args.chars();

            while let Some(short_arg) = short_args.next() {
                match short_arg {
                    'h' => {
                        println!("{HELP}");
                        return;
                    },
                    'i' => {
                        assert!(short_args.next().is_none());
                        ip = long_args.next().unwrap();
                    },
                    'm' => {
                        assert!(short_args.next().is_none());
                        endpoints.push(EncodedEndpoint::Multipart(Endpoint::from_string(long_args.next().unwrap())));
                    },
                    'n' => {
                        assert!(short_args.next().is_none());
                        NAME.get_or_init(|| long_args.next().unwrap());
                    },
                    'p' => {
                        assert!(short_args.next().is_none());
                        port = long_args.next().unwrap();
                    },
                    'u' => {
                        assert!(short_args.next().is_none());
                        endpoints.push(EncodedEndpoint::UrlEncoded(Endpoint::from_string(long_args.next().unwrap())));
                    },
                    #[cfg(target_os= "linux")]
                    'U' => {
                        assert!(short_args.next().is_none());
                        unix_socket = Some(long_args.next().unwrap());
                    },
                    'w' => {
                        assert!(short_args.next().is_none());
                        workers = long_args.next().unwrap().parse::<usize>().unwrap();
                    },
                    unknown => panic!("Unknown argument -{unknown}"),
                }
            }
        }
        else {
            panic!("Unknown argument {long_arg}");
        }
    }

    let mut app  = Router::new();

    for endpoint in endpoints {
        match endpoint {
            EncodedEndpoint::UrlEncoded(endpoint) => {
                let Endpoint { route, command, parameters, max_request_size, } = endpoint;
                let route = route.to_str().unwrap();

                app = app
                    .route(route, post(async |uri: Uri, form: Form<Vec<(String, String)>>| -> (StatusCode, HeaderMap, String) {
                        let route = uri.path();

                        let mut key_map = HashMap::<String, usize>::new();
                        let mut parameters_iter = parameters.into_iter();
                        let mut arguments = Vec::new();

                        for (key, value) in form.iter() {
                            if let Some(parameter) = parameters_iter.next() {
                                let Parameter { name, validator, } = parameter;

                                if &name != key {
                                    return err_400!("{route}: Unexpected parameter '{key}'");
                                }

                                key_map.insert(key.to_owned(), key_map.get(key).map(|v| *v + 1).unwrap_or(0));

                                arguments.push(value);

                                if let Some(validator) = validator {
                                    match Command::new(validator).args(&arguments).output() {
                                        Ok(o) => {
                                            if print_stderr(&o).is_err() {
                                                return err_500!("{route}: {name}: Failed to write to stderr while processing validator");
                                            }

                                            match o.status.code() {
                                                Some(code) => match code {
                                                    0 => {},
                                                    code => {
                                                        return err_400!("{route}: {name}: Validator exited with an unsuccessful status code ({code})");
                                                    },
                                                },
                                                None => {
                                                    return err_500!("{route}: {name}: Validator exited with an indeterminate status code");
                                                },
                                            }
                                        },
                                        Err(e) => {
                                            eprintln!("{route}: {name}: {e:?}");
                                            return err_500!("{route}: {name}: Validator execution returned an error");
                                        },
                                    }
                                }
                            }
                            else {
                                return err_400!("{route}: Unexpected field '{key}'");
                            }
                        }

                        if let Some(parameter) = parameters_iter.next() {
                            let Parameter { name, .. } = parameter;

                            return err_400!("{route}: Field '{name}' was missing from request");
                        }

                        match Command::new(command).args(&arguments).output() {
                            Ok(o) => {
                                if print_stderr(&o).is_err() {
                                    return err_500!("{route}: Failed to write handler stderr to stderr");
                                }

                                match o.status.code() {
                                    Some(code) => match code {
                                        0 => {},
                                        code => {
                                            return err_400!("{route}: Handle exited with an unsuccessful status code ({code})");
                                        },
                                    },
                                    None => {
                                        return err_500!("{route}: Handle exited with an indeterminate status code");
                                    },
                                }

                                status_headers_and_content(o.stdout)
                            },
                            Err(e) => {
                                eprintln!("{route}: {e:?}");
                                err_500!("{route}: Command execution returned an error")
                            },
                        }
                    }))
                    .layer(DefaultBodyLimit::max(max_request_size));
            },
            EncodedEndpoint::Multipart(endpoint) => {
                let Endpoint { route, command, parameters, max_request_size, } = endpoint;
                let route = route.to_str().unwrap();

                app = app
                    .route(route, post(async |uri: Uri, form: Option<Multipart>| -> (StatusCode, HeaderMap, String) {
                        let route = uri.path();

                        let mut form = if let Some(form) = form {
                            form
                        }
                        else {
                            return err_400!("{route}: Invalid request to multipart endpoint");
                        };

                        let tmp = err_500_res!(
                            TempDir::with_prefix(NAME.get_or_init(|| "postal.".to_owned())),
                            "{route}: Failed to create a temporary directory for request handling",
                        );

                        let mut parameters_iter = parameters.into_iter();
                        let mut key_map = HashMap::<String, usize>::new();
                        let mut arguments = Vec::new();

                        while let Some(field) = err_500_res!(
                            form.next_field().await,
                            "{route}: Failed to read the next form field from the multipart stream"
                        ) {
                            let key = err_500_opt!(field.name(), "{route}: Failed to read name of multipart form field").to_owned();
                            if let Some(parameter) = parameters_iter.next() {
                                let Parameter { name, validator, } = parameter;

                                if name != key {
                                    return err_400!("{route}: Unexpected parameter '{key}'");
                                }

                                key_map.insert(key.to_owned(), key_map.get(&key).map(|v| *v + 1).unwrap_or(0));
                                let filename = base26(*err_500_opt!(key_map.get(&key), "{route}: {name}: Failed to get value of '{key}' from map"));

                                let text = if field.content_type().is_some() {
                                    let bytes = err_500_res!(field.bytes().await, "{route}: {name}: Failed to read bytes of field '{key}'; file could be too large");

                                    let mut path: PathBuf = tmp.path().into();
                                    path.push(key);

                                    err_500_res!(create_dir_all(&path), "{route}: {name}: Failed to create directory '{path:?}'");

                                    path.push(filename);

                                    let mut file = err_500_res!(File::create(&path), "{route}: {name}: Failed to create file '{path:?}'");
                                    err_500_res!(file.write_all(&bytes), "{route}: {name}: Failed to write bytes to file '{path:?}'");
                                    drop(file);

                                    err_500_opt!(path.to_str(), "{route}: {name}: Failed to coerce path to string").to_owned()
                                }
                                else {
                                    err_500_res!(field.text().await, "{route}: {name}: Failed to read text of field '{key}'")
                                };

                                arguments.push(text);

                                if let Some(validator) = validator {
                                    match Command::new(validator).args(&arguments).output() {
                                        Ok(o) => {
                                            if print_stderr(&o).is_err() {
                                                return err_500!("{route}: {name}: Failed to write to stderr while processing validator");
                                            }

                                            match o.status.code() {
                                                Some(code) => match code {
                                                    0 => {},
                                                    code => {
                                                        return err_400!("{route}: {name}: Validator exited with an unsuccessful status code ({code})");
                                                    },
                                                },
                                                None => {
                                                    return err_500!("{route}: {name}: Validator exited with an indeterminate status code");
                                                },
                                            }
                                        },
                                        Err(e) => {
                                            eprintln!("{route}: {name}: {e:?}");
                                            return err_500!("{route}: {name}: Validator execution returned an error");
                                        },
                                    }
                                }
                            }
                            else {
                                return err_400!("{route}: Unexpected parameter '{key}'");
                            }
                        }

                        if let Some(parameter) = parameters_iter.next() {
                            let Parameter { name, .. } = parameter;
                            return err_400!("{route}: Field '{name}' was missing from request");
                        }

                        let tmp_path = err_500_opt!(tmp.path().to_str(), "{route}: Failed to get path from temporary directory").to_owned();
                        match Command::new(command).args(&arguments).output() {
                            Ok(o) => {
                                err_500_res!(tmp.close(), "{route}: Failed to close temporary directory {tmp_path:?}");
                                if print_stderr(&o).is_err() {
                                    return err_500!("{route}: Failed to write handler stderr to stderr");
                                }

                                match o.status.code() {
                                    Some(code) => match code {
                                        0 => {},
                                        code => {
                                            return err_400!("{route}: Handle exited with an unsuccessful status code ({code})");
                                        },
                                    },
                                    None => {
                                        return err_500!("{route}: Handle exited with an indeterminate status code");
                                    },
                                }

                                status_headers_and_content(o.stdout)
                            },
                            Err(e) => {
                                err_500_res!(tmp.close(), "{route}: Failed to close temporary directory {tmp_path:?}");
                                eprintln!("{e:?}");
                                err_500!("{route}: Command execution returned an error")
                            },
                        }
                    }))
                    .layer(DefaultBodyLimit::max(max_request_size));
            },
        }
    }

    #[cfg(target_os = "linux")]
    if let Some(unix_socket) = unix_socket {
        if workers > 0 {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = UnixListener::bind(&unix_socket).unwrap();
                    serve(listener, app).await.unwrap();
                });
        }
        else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap()
                .block_on(async {
                    let listener = UnixListener::bind(&unix_socket).unwrap();
                    serve(listener, app).await.unwrap();
                });
        }

        return;
    }

    if workers > 0 {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let listener = TcpListener::bind(&format!("{ip}:{port}"))
                    .await
                    .unwrap();

                serve(listener, app).await.unwrap();
            });
    }
    else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                let listener = TcpListener::bind(&format!("{ip}:{port}"))
                    .await
                    .unwrap();

                serve(listener, app).await.unwrap();
            });
    }
}
