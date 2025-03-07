use crate::error::E;
use crate::metadata::Metadata;
use crate::session::{get_sessions, mark_session_for_closure_uuid, user_connected, SessionData};
use askama::Template; // bring trait in scope
use bytes::Bytes;
use rust_embed::RustEmbed;
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use urlencoding::decode;
use warp::reply::Json;
use warp::{http::Response, Filter};
use warp_range::{filter_range, get_range};

#[derive(Template)]
#[template(path = "index.html", escape = "none")]
pub struct Index {
    sessions: Vec<SessionData>,
}

pub async fn index() -> std::result::Result<impl warp::Reply, warp::Rejection> {
    let mut sessions = get_sessions().await.ok_or(warp::reject::reject())?;
    sessions.sort_by(|a, b| {
        a.created_at
            .partial_cmp(&b.created_at)
            .expect("Unexpected error in comparison")
    });

    let template = Index { sessions };

    Ok(warp::reply::html(template.render().unwrap()))
}

#[derive(Template)]
#[template(path = "practice.html", escape = "none")]
pub struct PracticeData {
    metadata: Metadata,
    resource_path: String,
    lang: String,
}

pub async fn practice(
    resource_path: String,
    lang: String,
) -> std::result::Result<impl warp::Reply, warp::Rejection> {
    let metadata = match Metadata::from_resource_path(
        &decode(&resource_path)
            .expect("invalide URL encoding")
            .into_owned(),
    ) {
        Ok(m) => m,
        Err(e) => {
            log::error!("Error loading metadata in practise: {:?}", e);
            return Err(warp::reject::not_found());
        }
    };
    let template = PracticeData {
        metadata,
        resource_path,
        lang,
    };

    Ok(warp::reply::html(template.render().unwrap()))
}

#[derive(Template)]
#[template(path = "compare.html", escape = "none")]
pub struct Comparison {
    resource: String,
    uuid: String,
    lang: String,
}

pub async fn compare(
    resource_path: String,
    uuid: String,
    lang: String,
) -> std::result::Result<impl warp::Reply, warp::Rejection> {
    let template = match crate::compare::get_comparison(&resource_path, &uuid, &lang).await {
        Ok(c) => Comparison {
            resource: c.resource,
            uuid: c.uuid,
            lang: c.lang,
        },
        Err(e) => {
            log::error!("Couldn't get transcript for uuid {}: {:?}", uuid, e);
            return Err(warp::reject::reject());
        }
    };
    Ok(warp::reply::html(template.render().unwrap()))
}

pub async fn download_audio(
    uuid: String,
) -> std::result::Result<impl warp::Reply, warp::Rejection> {
    let session_id = crate::session::find_session_with_uuid(&uuid).await.unwrap();
    let session = crate::session::get_session(&session_id).await.unwrap();
    let content_path = session.recording_file.unwrap();
    log::debug!("content_path is {}", content_path);
    let mut f = std::fs::File::open(content_path.clone()).unwrap();
    let metadata = std::fs::metadata(&content_path).expect("unable to read metadata");
    let mut buffer = vec![0; metadata.len() as usize];
    let _ = f.read(&mut buffer).expect("buffer overflow");
    let b: Bytes = Bytes::from(buffer);
    let response = match Response::builder()
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}.wav\"", uuid),
        )
        .body(b)
    {
        Ok(b) => b,
        Err(e) => {
            log::error!("Error making response: {:?}", e);
            return Err(warp::reject::not_found());
        }
    };
    Ok(response)
}

pub async fn get_resource_filename(resource_path: String) -> E<String> {
    let metadata = match Metadata::from_resource_path(&resource_path) {
        Ok(m) => m,
        Err(e) => {
            log::error!(
                "Error in get_resource_filename: {:?} loading {}",
                e,
                resource_path
            );
            return Err(crate::error::Er::new(e.to_string()));
        }
    };
    let content_path = format!("{}/{}", metadata.enclosing_directory, metadata.audio);
    log::debug!("content_path is {}", content_path);
    Ok(content_path)
}

pub async fn serve() {
    let chat = warp::path("chat")
        .and(warp::query::<HashMap<String, String>>())
        .and(warp::ws())
        .map(move |params: HashMap<String, String>, ws: warp::ws::Ws| {
            let lang: String = (params.get("lang").unwrap_or(&"de".to_string())).clone();
            let resource: Option<String> = params.get("resource").cloned();
            let sample_rate: u32 = match params.get("rate") {
                Some(rate) => rate.to_string(),
                None => "44100".to_string(),
            }
            .parse()
            .unwrap();
            ws.on_upgrade(move |socket| user_connected(socket, lang, sample_rate, resource))
        });

    let close = warp::post().and(warp::path!("close" / String).and_then(|uuid| async move {
        mark_session_for_closure_uuid(uuid).await;
        Ok::<&str, warp::Rejection>("foo")
    }));

    let practice = warp::get().and(
        warp::path!("practice" / String / String)
            .and_then(|directory, lang| async move { practice(directory, lang).await }),
    );

    let serve_resource = warp::get().and(
        warp::path!("serve_resource" / String)
            .and(filter_range())
            .and_then(|resource_path: String, range_header| async move {
                let filename = get_resource_filename(
                    decode(&resource_path)
                        .expect("Invalid source path in serve_resource")
                        .into_owned(),
                )
                .await
                .unwrap();
                let mime_type = mime_guess::from_path(&filename).first().unwrap();
                log::debug!("Found MIME type {}", mime_type.as_ref());
                get_range(range_header, &filename, mime_type.as_ref()).await
            }),
    );

    let status = warp::path!("status" / String).and_then(|uuid| async move {
        match crate::session::find_session_with_uuid(&uuid).await {
            Some(session_id) => match crate::session::get_session(&session_id).await {
                Some(session) => {
                    Ok::<Json, warp::Rejection>(warp::reply::json(&session.status().unwrap()))
                }
                None => Err(warp::reject::not_found()),
            },
            None => Err(warp::reject::not_found()),
        }
    });

    let compare = warp::get()
        .and(warp::path!("compare" / String / String / String))
        .and_then(|resource_path: String, uuid, lang| async move {
            match compare(
                decode(&resource_path)
                    .expect("Invalid URL encoding in compare")
                    .into_owned(),
                uuid,
                lang,
            )
            .await
            {
                Ok(x) => Ok(x),
                Err(e) => {
                    log::error!("Error in compare: {:?}", e);
                    Err(warp::reject())
                }
            }
        });

    let changes = warp::get()
        .and(warp::path!("changes" / String / String / String))
        .and_then(|resource_path: String, uuid, lang| async move {
            match crate::compare::changes(decode(&resource_path).expect("Invlude URL encoding in changes").into_owned(), uuid, lang).await {
                Ok(x) => {
                    let changes = x.clone();
                    let reply = warp::reply::json(&changes);
                    Ok(reply)
                }
                Err(e) => {
                    log::error!("Error in changes: {:?}", e);
                    Err(warp::reject())
                }
            }
        });

    let recording = warp::get()
        .and(warp::path!("recording" / String))
        .and_then(|uuid| async { download_audio(uuid).await });

    let assets_dir = std::env::var("ASSETS_DIR").unwrap_or("../assets".to_string());
    let assets = warp::get()
        .and(warp::path("assets"))
        .and(warp::fs::dir(assets_dir));

    let transcript = warp::path!("transcript" / String).and_then(|uuid| async move {
        match crate::session::find_session_with_uuid(&uuid).await {
            Some(session_id) => match crate::session::get_session(&session_id).await {
                Some(session) => Ok(session.transcript().unwrap()),
                None => Err(warp::reject::not_found()),
            },
            None => Err(warp::reject::not_found()),
        }
    });

    let index = warp::path::end().and_then(|| async move { crate::api::index().await });

    #[derive(RustEmbed)]
    #[folder = "../client"]
    struct StaticContent;
    let static_content_serve = warp_embed::embed(&StaticContent);

    let routes = index
        .or(assets)
        .or(changes)
        .or(chat)
        .or(close)
        .or(compare)
        .or(practice)
        .or(recording)
        .or(serve_resource)
        .or(status)
        .or(static_content_serve)
        .or(transcript);
    log::debug!("Starting server");
    let listen;
    if let Ok(x) = std::env::var(" LISTEN") {
        listen = x.parse().unwrap();
    } else {
        listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 3030);
    };

    warp::serve(routes).run(listen).await;
}
