//
// Shared library for sending mail via Gmail
//

use std::ffi::{c_char, CString};
use serde::{Deserialize, Serialize};
use hyper::HeaderMap;
use lettre::transport::smtp;
use lettre::message::{Attachment, MultiPart, SinglePart, header::ContentType};
use lettre::{Message, SmtpTransport, Transport};
use once_cell::sync::Lazy;
use multer::Multipart;
use tokio::runtime::Runtime;
use futures_util::stream::once;
use bytes::Bytes;


static VERSION: &'static str = "0.1.0";

#[derive(Debug, Serialize)]
struct PluginRoute {
    path: &'static str,
    function: &'static str,
    method_router: &'static str,
    response_type: &'static str,
}

static ROUTES: &[PluginRoute] = &[
    PluginRoute {
        path: "/sendmail",
        function: "sendmail",
        method_router: "post",
        response_type: "json",
    },
    PluginRoute {
        path: "/about",
        function: "about",
        method_router: "get",
        response_type: "text",
    },
];

#[derive(Clone, Deserialize, Serialize)]
struct Mail {
    from: String,
    to: String,
    cc: Option<String>,
    bcc: Option<String>,
    reply_to: Option<String>,
    sender_name: Option<String>,
    sender_email: Option<String>,
    subject: String,
    message: String,
}

#[derive(Clone, Deserialize)]
struct SmtpSettings {
    username: String,
    password: String,
    server: String,
    max_size_str: Option<String>,
}

#[derive(Clone, Serialize)]
struct Response {
    status: String,
    message: String,
}

trait Pipe<T> {
    fn pipe<U, F>(self, f: F) -> U where F: FnOnce(T) -> U;
}

impl<T> Pipe<T> for T {
    fn pipe<U, F>(self, f: F) -> U where F: FnOnce(T) -> U {
        f(self)
    }
}

static SMTP_CLIENT: Lazy<SmtpSettings> = Lazy::new(|| {
    let config_file = std::env::var("PLUGINS_DIR")
        .unwrap_or_else(|_| "plugins".to_string())
        .pipe(|dir| std::path::Path::new(&dir).join("rcp-gmail/config.toml"));

    let config_content = match std::fs::read_to_string(&config_file) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("Failed to read config file {}: {}", config_file.display(), e);
            return SmtpSettings {
                username: String::new(),
                password: String::new(),
                server: String::new(),
                max_size_str: None,
            };
        }
    };

    match toml::from_str(&config_content) {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!("Failed to parse config.toml: {}", e);
            SmtpSettings {
                username: String::new(),
                password: String::new(),
                server: String::new(),
                max_size_str: None,
            }
        }
    }
});

fn parse_size(size_str: &str) -> Option<u64> {
    let size_str = size_str.trim().to_uppercase();
    let mut number_part = String::new();
    let mut unit_part = String::new();

    for c in size_str.chars() {
        if c.is_digit(10) {
            number_part.push(c);
        } else {
            unit_part.push(c);
        }
    }

    let number: u64 = number_part.parse().ok()?;
    let multiplier = match unit_part.as_str() {
        "B" => 1,
        "KB" => 1024,
        "MB" => 1024 * 1024,
        "GB" => 1024 * 1024 * 1024,
        "TB" => 1024_u64.pow(4),
        _ => return None, // Unknown unit
    };

    Some(number * multiplier)
}

fn to_c_response(r: &Response) -> *const c_char {
    serde_json::to_string(r) // Removed pretty printing for simplicity
        .map(|s| CString::new(s).unwrap().into_raw())
        .unwrap_or_else(|e| {
            let error_response = Response {
                status: "error".to_string(),
                message: format!("Serialization error: {}", e),
            };
            CString::new(serde_json::to_string(&error_response).unwrap())
                .unwrap()
                .into_raw()
        })
}

fn send_via_gmail(
    mail: &Mail,
    attachment: Option<(String, Vec<u8>)>,
) -> Result<smtp::response::Response, String> {
    let mut email_builder = Message::builder()
        .from(mail.from.parse().map_err(|e| format!("Invalid from address: {}", e))?)
        .to(mail.to.parse().map_err(|e| format!("Invalid to address: {}", e))?)
        .subject(&mail.subject);

    if let Some(cc) = &mail.cc {
        email_builder = email_builder.cc(cc.parse().map_err(|e| format!("Invalid cc address: {}", e))?);
    }
    if let Some(bcc) = &mail.bcc {
        email_builder = email_builder.bcc(bcc.parse().map_err(|e| format!("Invalid bcc address: {}", e))?);
    }
    if let Some(reply_to) = &mail.reply_to {
        email_builder = email_builder.reply_to(reply_to.parse().map_err(|e| format!("Invalid reply-to address: {}", e))?);
    }

    let email = if let Some((filename, data)) = attachment {
        email_builder
            .multipart(
                MultiPart::mixed()
                    .singlepart(
                        SinglePart::builder()
                            .header(ContentType::TEXT_PLAIN)
                            .body(mail.message.clone()),
                    )
                    .singlepart(
                        Attachment::new(filename)
                            .body(data, ContentType::parse("application/octet-stream").unwrap()),
                    ),
            )
            .map_err(|e| format!("Failed to build email with attachment: {}", e))?
    } else {
        email_builder
            .singlepart(
                SinglePart::builder()
                    .header(ContentType::TEXT_PLAIN)
                    .body(mail.message.clone()),
            )
            .map_err(|e| format!("Failed to build email: {}", e))?
    };

    let credentials = smtp::authentication::Credentials::new(
        SMTP_CLIENT.username.clone(),
        SMTP_CLIENT.password.clone(),
    );

    let mailer = SmtpTransport::relay(&SMTP_CLIENT.server)
        .map_err(|e| format!("SMTP connection failed: {}", e))?
        .credentials(credentials)
        .build();

    mailer.send(&email)
        .map_err(|e| format!("Failed to send email: {}", e))
}


#[unsafe(no_mangle)]
pub extern "C" fn sendmail(headers: *const HeaderMap, body: *const u8, body_len: usize) -> *const c_char {
    let mut response = Response {
        status: "error".to_string(),
        message: "Internal plugin error".to_string(),
    };

    if headers.is_null() || body.is_null() || body_len == 0 {
        response.message = "Null pointer or empty body received".to_string();
        return to_c_response(&response);
    }

    let headers = unsafe { &*headers };
    // Convert raw body pointer to a byte slice
    let body_slice = unsafe { std::slice::from_raw_parts(body, body_len) };

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.starts_with("multipart/form-data") {
        response.message = format!("Expected multipart/form-data, got: {}", content_type);
        return to_c_response(&response);
    }

    let boundary = match content_type.split("boundary=").nth(1) {
        Some(s) => s.trim_matches('"'),
        None => {
            response.message = "Missing boundary in content-type".to_string();
            return to_c_response(&response);
        }
    };

    let mut mail = Mail {
        from: String::new(),
        to: String::new(),
        subject: String::new(),
        message: String::new(),
        cc: None,
        bcc: None,
        reply_to: None,
        sender_name: None,
        sender_email: None,
    };
    let mut attachment: Option<(String, Vec<u8>)> = None;

    let rt = Runtime::new().unwrap();
    let result: Result<(), multer::Error> = rt.block_on(async {
        // Use body_slice directly as Bytes
        let body_bytes = Bytes::from(body_slice.to_vec());
        let stream = once(async { Ok::<Bytes, std::io::Error>(body_bytes) });
        let mut multipart = Multipart::new(stream, boundary);

        while let Some(field) = multipart.next_field().await? {
            let name = match field.name() {
                Some(n) => n.to_string(),
                None => continue,
            };

            match name.as_str() {
                "from" => mail.from = field.text().await.unwrap_or_default(),
                "to" => mail.to = field.text().await.unwrap_or_default(),
                "subject" => mail.subject = field.text().await.unwrap_or_default(),
                "message" => mail.message = field.text().await.unwrap_or_default(),
                "attachment" => {
                    let filename = field.file_name().map(|f| f.to_string());
                    if let Some(filename) = filename {
                        let data = field.bytes().await.unwrap_or_default().to_vec();
                        attachment = Some((filename, data));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    });

    if let Err(e) = result {
        response.message = format!("Failed to parse multipart data: {}", e);
        return to_c_response(&response);
    }

    if mail.from.is_empty() || mail.to.is_empty() || mail.subject.is_empty() || mail.message.is_empty() {
        response.message = "Missing required field".to_string();
        return to_c_response(&response);
    }

    if let Some((_, data)) = &attachment {
        let max_size = match parse_size(SMTP_CLIENT.max_size_str.as_deref().unwrap_or("25MB")) {
            Some(size) => size,
            None => 25 * 1024 * 1024,
        };
        if data.len() > max_size as usize {
            response.message = format!("Attachment exceeds {} limit", max_size);
            return to_c_response(&response);
        }
    }

    match send_via_gmail(&mail, attachment) {
        Ok(_) => {
            response.status = "success".to_string();
            response.message = "Email sent successfully".to_string();
        }
        Err(e) => response.message = e,
    }

    to_c_response(&response)
}

#[unsafe(no_mangle)]
pub extern "C" fn routes() -> *const c_char {
    CString::new(serde_json::to_string(ROUTES).unwrap_or_else(|_| "[]".to_string()))
        .unwrap()
        .into_raw()
}

// curl -X GET http://0.0.0.0:8080/plugin/rcp-gmail/about
#[unsafe(no_mangle)]
pub extern "C" fn about(_headers: *const HeaderMap, _body: *const u8, _body_len: usize) -> *const c_char {
    let info = format!(
        "Name: rcp-gmail\nVersion: {}\nauthors = \"Henrique Dias <mrhdias@gmail.com>\"\nDescription: Shared library for sending mail via Gmail\nLicense: MIT",
        VERSION
    );
    CString::new(info).unwrap().into_raw()
}

#[unsafe(no_mangle)]
pub extern "C" fn free(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            drop(CString::from_raw(ptr));
        }
    }
}
