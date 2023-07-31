//! Very basic remote analyse / upload / download server for `usbsas` using `clamav`.
//! Mainly used for example and integration tests.

use actix_files::NamedFile;
use actix_web::{get, head, http::header, post, web, App, HttpResponse, HttpServer, Responder};
use clamav_rs::{
    db, engine,
    scan_settings::{ScanSettings, ScanSettingsBuilder},
};
use clap::{Arg, Command};
use futures::StreamExt;
use serde_json::json;
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
};
use tar::Archive;
use tempfile::TempDir;

struct AnalyzeStatus {
    status: String,
    files: HashMap<String, String>,
}

struct AppState {
    working_dir: Mutex<String>,
    current_scans: Mutex<HashMap<String, AnalyzeStatus>>,
    clamav_engine: Mutex<engine::Engine>,
    clamav_settings: Mutex<ScanSettings>,
}

impl AppState {
    fn analyze(&self, bundle_id: String, tar: String) -> Result<(), actix_web::Error> {
        let tmpdir = tempfile::Builder::new()
            .prefix(&bundle_id)
            .tempdir_in(PathBuf::from(&*self.working_dir.lock().unwrap()))
            .unwrap();
        let mut archive = Archive::new(fs::File::open(tar).unwrap());
        // XXX TODO maybe mmap archive file and use clamav's scan_map function instead of unpacking
        if let Err(err) = archive.unpack(tmpdir.path()) {
            log::error!("err: {}, not a tar ?", err);
            self.current_scans
                .lock()
                .unwrap()
                .get_mut(&bundle_id)
                .unwrap()
                .status = "error".to_string();
            drop(archive);
            return Ok(());
        }

        // If we received a bundle (with "/data" and "/config.json" at the root), only analyze the
        // data directory.
        let base_path = if tmpdir.path().join("data").as_path().is_dir()
            && tmpdir.path().join("config.json").as_path().is_file()
        {
            tmpdir.path().join("data")
        } else {
            tmpdir.path().to_path_buf()
        };

        self.analyze_recursive(
            base_path.as_path(),
            base_path.as_path().to_str().unwrap(),
            &bundle_id,
        )?;

        self.current_scans
            .lock()
            .unwrap()
            .get_mut(&bundle_id)
            .unwrap()
            .status = "scanned".to_string();

        Ok(())
    }

    fn analyze_recursive<P: AsRef<Path>>(
        &self,
        path: P,
        base_path: &str,
        bundle_id: &str,
    ) -> Result<(), actix_web::Error> {
        for file in fs::read_dir(path).unwrap() {
            let file = file.unwrap();
            let file_type = file.file_type().unwrap();
            let filename = file.path().into_os_string().into_string().unwrap();
            let relative_filename = filename
                .strip_prefix(&format!("{base_path}/"))
                .unwrap()
                .to_string();
            if file_type.is_symlink() {
                self.current_scans
                    .lock()
                    .unwrap()
                    .get_mut(bundle_id)
                    .unwrap()
                    .files
                    .insert(relative_filename, "CLEAN".to_string());
            } else if file_type.is_dir() {
                self.analyze_recursive(file.path(), base_path, bundle_id)?;
            } else {
                let scan_res = self
                    .clamav_engine
                    .lock()
                    .unwrap()
                    .scan_file(&filename, &mut self.clamav_settings.lock().unwrap());
                let mut current_scans = self.current_scans.lock().unwrap();
                match scan_res {
                    Ok(engine::ScanResult::Clean) | Ok(engine::ScanResult::Whitelisted) => {
                        log::debug!("Clean or whitelisted file: {}", &relative_filename);
                        current_scans
                            .get_mut(bundle_id)
                            .unwrap()
                            .files
                            .insert(relative_filename, "CLEAN".to_string());
                    }
                    Ok(engine::ScanResult::Virus(vname)) => {
                        log::warn!("Dirty file: {}, reason: {}", &relative_filename, vname);
                        current_scans
                            .get_mut(bundle_id)
                            .unwrap()
                            .files
                            .insert(relative_filename, "DIRTY".to_string());
                    }
                    Err(err) => {
                        log::error!("scan error: {}", err);
                        current_scans
                            .get_mut(bundle_id)
                            .unwrap()
                            .files
                            .insert(relative_filename, "DIRTY".to_string());
                    }
                }
            }
        }
        Ok(())
    }

    async fn recv_file(
        &self,
        mut body: web::Payload,
    ) -> Result<(String, String), actix_web::Error> {
        #[cfg(not(feature = "integration-tests"))]
        let bundle_id = uuid::Uuid::new_v4().simple().to_string();
        #[cfg(feature = "integration-tests")]
        let bundle_id = "bundle_test".into();
        let out_file_name = format!("{}/{}.tar", self.working_dir.lock().unwrap(), bundle_id);
        let mut out_file = fs::File::create(out_file_name.clone()).unwrap();

        while let Some(bytes) = body.next().await {
            let bytes = bytes?;
            out_file.write_all(&bytes).unwrap();
        }
        out_file.flush().unwrap();
        Ok((bundle_id, out_file_name))
    }
}

#[post("/api/scanbundle/{id}")]
async fn scan_bundle(
    body: web::Payload,
    _id: web::Path<String>,
    data: web::Data<AppState>,
) -> Result<impl Responder, actix_web::Error> {
    let (bundle_id, out_file_name) = data.recv_file(body).await?;

    data.current_scans.lock().unwrap().insert(
        bundle_id.clone(),
        AnalyzeStatus {
            status: "processing".to_string(),
            files: HashMap::new(),
        },
    );

    let bundle_id_clone = bundle_id.clone();
    thread::spawn(move || {
        let _ = data.analyze(bundle_id_clone, out_file_name);
    });

    Ok(HttpResponse::Ok().json(json!(
        {
            "id": bundle_id,
            "status": "uploaded"
        }
    )))
}

#[get("/api/scanbundle/{id}/{bundle_id}")]
async fn scan_result(
    params: web::Path<(String, String)>,
    data: web::Data<AppState>,
) -> Result<impl Responder, actix_web::Error> {
    let (_, bundle_id) = params.into_inner();
    let mut current_scans = data.current_scans.lock().unwrap();
    if current_scans.contains_key(&bundle_id) {
        let rep = if current_scans[&bundle_id].status == "scanned"
            || current_scans[&bundle_id].status == "error"
        {
            let entry = current_scans.remove(&bundle_id).unwrap();
            #[cfg(not(feature = "integration-tests"))]
            let av_infos = json!({
                "ClamAV": {
                    "version": clamav_rs::version(),
                    "database_version": data.clamav_engine
                        .lock().unwrap().database_version().unwrap(),
                    "database_timestamp": data.clamav_engine
                        .lock().unwrap().database_timestamp().unwrap()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .unwrap().as_secs_f64()
                }
            });
            // Fixed timestamp to keep a determistic filesystem hash
            #[cfg(feature = "integration-tests")]
            let av_infos = json!({
                "ClamAV": {
                    "version": "946767600",
                    "database_version": "946767600",
                    "database_timestamp": "946767600",
                }
            });
            fs::remove_file(format!(
                "{}/{}.tar",
                data.working_dir.lock().unwrap(),
                bundle_id
            ))
            .unwrap();
            #[cfg(feature = "integration-tests")]
            let bundle_id = "0";
            let files_status: HashMap<String, serde_json::Value> = entry
                .files
                .iter()
                .map(|(key, val)| (key.clone(), json!({ "status": val })))
                .collect();
            json!({
                "id": bundle_id,
                "status": entry.status,
                "version": 2,
                "files": files_status,
                "antivirus": av_infos
            })
        } else {
            json!({
                "id": bundle_id,
                "status": current_scans[&bundle_id].status,
            })
        };
        Ok(HttpResponse::Ok().json(rep))
    } else {
        Ok(HttpResponse::NotFound().finish())
    }
}

#[post("/api/uploadbundle/{id}")]
async fn upload_bundle(
    body: web::Payload,
    _id: web::Path<String>,
    data: web::Data<AppState>,
) -> Result<impl Responder, actix_web::Error> {
    let (_, _) = data.recv_file(body).await?;
    Ok(HttpResponse::Ok())
}

fn find_bundle(filename: &str) -> Result<(String, u64), actix_web::Error> {
    for ext in ["tar", "tar.gz", "gz"] {
        let bundle_path = format!("{filename}.{ext}");
        if let Ok(metadata) = fs::metadata(&bundle_path) {
            return Ok((bundle_path, metadata.len()));
        }
    }
    Err(actix_web::error::ErrorNotFound(io::Error::new(
        io::ErrorKind::NotFound,
        "Bundle not found",
    )))
}

#[head("/api/downloadbundle/{id}/{bundle_id}")]
async fn head_bundle_size(
    params: web::Path<(String, String)>,
    data: web::Data<AppState>,
) -> Result<impl Responder, actix_web::Error> {
    let (id, bundle_id) = params.into_inner();
    let (bundle_path, mut size) = find_bundle(&format!(
        "{}/{}/{}",
        data.working_dir.lock().unwrap(),
        id,
        bundle_id
    ))?;

    // usbsas expects uncompressed size of files with HEAD requests

    // XXX FIXME Dirty hack 1:
    // /!\ The following only work if the gzipped tar is < 4GB (as it's stored
    // %4GB), but good enough for this integration tests server
    if bundle_path.ends_with("gz") {
        let mut f = fs::File::open(&bundle_path)?;
        f.seek(io::SeekFrom::End(-4)).unwrap();
        let mut buf = vec![0; 4];
        f.read_exact(&mut buf).unwrap();
        size = u32::from_ne_bytes(buf[0..4].try_into().unwrap()) as u64;
        log::debug!("filename: {}, uncompressed size: {}", bundle_path, size);
    }
    log::debug!("filename: {}, size: {}", bundle_path, size);

    Ok(HttpResponse::Ok()
        .insert_header(("X-Uncompressed-Content-Length", size))
        .finish())
}

#[get("/api/downloadbundle/{id}/{bundle_id}")]
async fn download_bundle(
    params: web::Path<(String, String)>,
    data: web::Data<AppState>,
) -> Result<impl Responder, actix_web::Error> {
    let (id, bundle_id) = params.into_inner();
    let (bundle_path, _) = find_bundle(&format!(
        "{}/{}/{}",
        data.working_dir.lock().unwrap(),
        id,
        bundle_id
    ))?;
    let named_file = if bundle_path.ends_with("gz") {
        NamedFile::open(bundle_path)?.set_content_encoding(header::ContentEncoding::Gzip)
    } else {
        NamedFile::open(bundle_path)?
    };
    Ok(named_file)
}

fn init_clamav() -> (engine::Engine, ScanSettings) {
    clamav_rs::initialize().expect("couldn't init clamav");
    let settings = ScanSettingsBuilder::new().build();
    let engine = engine::Engine::new();
    engine
        .load_databases(&db::default_directory())
        .expect("clamav database load failed");
    engine.compile().expect("clamav compile failed");
    log::info!("clamav initialized, starting server");
    (engine, settings)
}

#[actix_web::main]
async fn main() -> io::Result<()> {
    env_logger::init_from_env(env_logger::Env::default().filter_or("RUST_LOG", "info"));

    let command = Command::new("usbsas-analyzer-server")
        .about("simple usbsas remote server for integrations tests")
        .version("0.1")
        .arg(
            Arg::new("working-dir")
                .value_name("WORKING-DIR")
                .short('d')
                .long("working-dir")
                .num_args(1)
                .required(false),
        );

    let matches = command.get_matches();

    let (working_path, _temp_dir): (String, Option<TempDir>) =
        if let Some(wp) = matches.get_one::<String>("working-dir") {
            (wp.to_owned(), None)
        } else {
            let tmpdir = tempfile::Builder::new()
                .prefix("usbsas-analyzer")
                .tempdir_in("/tmp")
                .unwrap();
            let working_path = tmpdir.path().to_string_lossy().to_string();
            (working_path, Some(tmpdir))
        };

    let (engine, settings) = init_clamav();
    let app_data = web::Data::new(AppState {
        working_dir: Mutex::new(working_path),
        current_scans: Mutex::new(HashMap::new()),
        clamav_engine: Mutex::new(engine),
        clamav_settings: Mutex::new(settings),
    });
    HttpServer::new(move || {
        App::new()
            .app_data(app_data.clone())
            .service(scan_bundle)
            .service(scan_result)
            .service(upload_bundle)
            .service(head_bundle_size)
            .service(download_bundle)
    })
    .bind("127.0.0.1:8042")?
    .run()
    .await
}
