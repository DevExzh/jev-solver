use std::{
    collections::BTreeSet,
    io::Cursor,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};

use jev_solver::{
    client::JevClient,
    config::Credentials,
    solver::{Solver, Thresholds},
};

#[derive(Default)]
pub struct MockState {
    pub calls: AtomicUsize,
    pub scores: Vec<Vec<f64>>,
    pub status: Option<u16>,
    pub retry_first: bool,
    pub stale: bool,
    pub check_visible_images: bool,
    pub omit_refinement: bool,
    pub check_no_motion: bool,
    pub animation_finished: AtomicBool,
    pub clicks: Mutex<Vec<usize>>,
}

pub struct Server {
    pub url: String,
    pub state: Arc<MockState>,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    pub async fn start(state: MockState) -> Self {
        let state = Arc::new(state);
        let app = Router::new()
            .route("/v1/systemone", post(evaluate))
            .route("/fixture", get(host))
            .route("/recaptcha/api2/anchor", get(anchor))
            .route("/recaptcha/api2/bframe", get(challenge))
            .route("/tile/{id}", get(tile))
            .route("/version", get(version))
            .route("/click/{index}", post(click))
            .route("/motion-finished", post(motion_finished))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, state, task }
    }

    pub fn solver(&self, timeout: Duration, retries: usize) -> Solver {
        let credentials =
            Credentials::new(&format!("{}/v1/systemone", self.url), "test-key".to_owned()).unwrap();
        let client = JevClient::new(credentials, timeout, retries).unwrap();
        Solver::new(client, "jev-1.14.0".to_owned(), Thresholds::default()).unwrap()
    }
}

async fn evaluate(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> axum::response::Response {
    if state.check_no_motion {
        assert!(
            state.animation_finished.load(Ordering::SeqCst),
            "Inference began while the table was still moving"
        );
    }
    assert_eq!(headers["authorization"], "Bearer test-key");
    assert_eq!(headers["typesafe-danger-zone-images"], "true");
    let first = multipart.next_field().await.unwrap().unwrap();
    assert_eq!(first.name(), Some("request"));
    assert_eq!(first.content_type(), Some("application/json"));
    let request: Value = serde_json::from_slice(&first.bytes().await.unwrap()).unwrap();
    assert_eq!(request["model"], "jev-1.14.0");
    let mut parts = BTreeSet::new();
    while let Some(field) = multipart.next_field().await.unwrap() {
        assert_eq!(field.content_type(), Some("image/png"));
        let name = field.name().unwrap().to_owned();
        assert!(parts.insert(name));
        let bytes = field.bytes().await.unwrap();
        let image = image::load_from_memory(&bytes).unwrap();
        if state.check_visible_images && parts.len() == 1 {
            let pixel = image.to_rgb8().get_pixel(50, 50).0;
            assert!(
                pixel.iter().map(|channel| u32::from(*channel)).sum::<u32>() < 650,
                "Captured a washed-out replacement image before the overlay finished fading"
            );
        }
        assert!((256..=1568).contains(&image.width()));
        assert!((256..=1568).contains(&image.height()));
        assert!(image.width() * image.height() >= 78_400);
    }
    assert_eq!(parts.len(), request["assets"].as_object().unwrap().len());
    for asset in request["assets"].as_object().unwrap().values() {
        assert_eq!(asset["type"], "image");
        assert!(parts.contains(asset["part"].as_str().unwrap()));
    }
    let call = state.calls.fetch_add(1, Ordering::SeqCst);
    if state.retry_first && call == 0 {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "0")],
            "rate limited",
        )
            .into_response();
    }
    if let Some(status) = state.status {
        return (
            StatusCode::from_u16(status).unwrap(),
            "provider echo: Bearer test-key",
        )
            .into_response();
    }
    if state.stale && call == 0 {
        tokio::time::sleep(Duration::from_millis(850)).await;
    }
    let scores = state
        .scores
        .get(call)
        .or_else(|| state.scores.last())
        .unwrap();
    let mut answers: serde_json::Map<String, Value> = request["questions"]
        .as_object()
        .unwrap()
        .keys()
        .map(|key| {
            let index: usize = key.strip_prefix("tile_").unwrap().parse().unwrap();
            (
                key.clone(),
                json!({"type": "noul", "noul": scores[index - 1]}),
            )
        })
        .collect();
    if state.omit_refinement
        && call > 0
        && let Some(key) = answers.keys().next().cloned()
    {
        answers.remove(&key);
    }
    axum::Json(json!({"model": "jev-1.14.0", "answers": answers,
        "usage": {"input_tokens": 1000, "output_tokens": 90}}))
    .into_response()
}

pub fn png(width: u32, height: u32) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| {
        Rgb([(x % 256) as u8, (y % 256) as u8, 100])
    });
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

async fn host(Query(query): Query<std::collections::HashMap<String, String>>) -> Html<String> {
    let mode = query.get("mode").map(String::as_str).unwrap_or("static");
    let initial_token = if mode == "checked" {
        "fixture-token"
    } else {
        ""
    };
    Html(format!(
        r#"<!doctype html><html lang="en"><title>Local image-grid fixture</title>
<h1>Local solver fixture</h1><textarea name="g-recaptcha-response" hidden>{initial_token}</textarea>
<iframe id="anchor" src="/recaptcha/api2/anchor?mode={mode}" style="width:310px;height:85px;border:0"></iframe>
<iframe id="challenge" src="/recaptcha/api2/bframe?mode={mode}" style="display:none;width:460px;height:680px;border:0"></iframe>
<script>let fixtureToken=''; window.grecaptcha={{getResponse:()=>fixtureToken}};
addEventListener('message', event => {{
 if (event.data === 'show') {{
 const frame=document.querySelector('#challenge'); frame.style.display='block'; frame.style.position=''; frame.style.top='';
 frame.contentWindow.postMessage('show','*'); }}
 if (event.data === 'hide') {{const frame=document.querySelector('#challenge');frame.style.position='absolute';frame.style.top='-10000px';
 document.querySelector('#anchor').contentWindow.postMessage('unchecked','*');}}
 if (event.data === 'solved') {{
 if ('{mode}'==='callback') setTimeout(()=>{{fixtureToken='fixture-callback-token';}},300);
 else document.querySelector('textarea').value='fixture-token';
 document.querySelector('#anchor').contentWindow.postMessage('solved', '*');
 document.querySelector('#challenge').style.display='none'; }}
}});</script></html>"#
    ))
}

async fn anchor(Query(query): Query<std::collections::HashMap<String, String>>) -> Html<String> {
    let checked = query
        .get("mode")
        .is_some_and(|mode| mode == "checked" || mode == "missing-token");
    Html(format!(
        r#"<!doctype html><button id="recaptcha-anchor" role="checkbox" aria-checked="{checked}" onclick="parent.postMessage('show','*')">I'm not a robot</button>
<script>addEventListener('message',e=>{{ if(e.data==='solved') document.querySelector('button').setAttribute('aria-checked','true');
if(e.data==='unchecked') document.querySelector('button').setAttribute('aria-checked','false'); }});</script>"#
    ))
}

async fn challenge(Query(query): Query<std::collections::HashMap<String, String>>) -> Html<String> {
    let mode = query.get("mode").map(String::as_str).unwrap_or("static");
    if mode == "blocked" {
        return Html("<div class='rc-doscaptcha-body'>Automated queries</div>".to_owned());
    }
    if mode == "audio" {
        return Html(
            "<div class='rc-audiochallenge-instructions'>Type what you hear</div>".to_owned(),
        );
    }
    let size = if mode == "four" { 4 } else { 3 };
    let dynamic = mode == "dynamic";
    let mut cells = String::new();
    for row in 0..size {
        cells.push_str("<tr>");
        for col in 0..size {
            let index = row * size + col + 1;
            cells.push_str(&format!(
                "<td data-index='{index}'><img src='/tile/initial-{index}'><div class='rc-image-tile-overlay'></div></td>"
            ));
        }
        cells.push_str("</tr>");
    }
    let instruction = if dynamic {
        "Select all images with red squares. Click verify once there are none left."
    } else {
        "Select all squares with red squares"
    };
    let expected = match mode {
        "four" => "[1,6,16]",
        "stale" | "reject" => "[2]",
        "dynamic" => "[]",
        _ => "[1,5,9]",
    };
    Html(format!(
        r#"<!doctype html><html lang="en"><style>
table{{border-collapse:collapse;width:400px;height:400px;table-layout:fixed}}td{{padding:0;position:relative;cursor:pointer}}
img{{width:100%;height:100%;display:block}}.rc-imageselect-tileselected{{outline:4px solid #08f;outline-offset:-4px}}
.rc-image-tile-overlay{{position:absolute;inset:0;background:white;opacity:0;pointer-events:none;transition:opacity 600ms}}
</style><div class="rc-imageselect-desc-wrapper">{instruction}</div>
<table class="rc-imageselect-table-{size}{size}">{cells}</table>
<button id="recaptcha-verify-button">Verify</button><div class="rc-imageselect-incorrect-response" style="display:none">Wrong selection</div>
<button id="recaptcha-reload-button">Reload</button>
<script>
const dynamic={dynamic}; const mode='{mode}'; let changed=false;
document.querySelectorAll('td').forEach(cell=>cell.addEventListener('click',()=>{{
 const index=Number(cell.dataset.index); fetch('/click/'+index,{{method:'POST'}});
 if(dynamic){{ cell.classList.add('rc-imageselect-dynamic-selected');
 cell.querySelector('.rc-image-tile-overlay').style.opacity='1';
 setTimeout(()=>cell.classList.remove('rc-imageselect-dynamic-selected'),180);
 setTimeout(()=>{{cell.querySelector('img').src='/tile/replacement-'+index;cell.classList.remove('rc-imageselect-tileselected');}},650);
 setTimeout(()=>{{cell.querySelector('.rc-image-tile-overlay').style.opacity='0';}},1000);
 }}else cell.classList.toggle('rc-imageselect-tileselected');
}}));
document.querySelector('button').onclick=()=>{{
 const actual=[...document.querySelectorAll('.rc-imageselect-tileselected')].map(c=>Number(c.dataset.index));
 if(JSON.stringify(actual)===JSON.stringify({expected})) parent.postMessage('solved','*');
 else document.querySelector('.rc-imageselect-incorrect-response').style.display='block';
}};
let reload=0;
document.querySelector('#recaptcha-reload-button').onclick=()=>{{
 reload++; document.querySelector('.rc-imageselect-incorrect-response').style.display='none';
 document.querySelectorAll('td').forEach(cell=>{{cell.classList.remove('rc-imageselect-tileselected');
 cell.querySelector('img').src='/tile/reloaded-'+reload+'-'+cell.dataset.index;}});
}};
if(mode==='stale'||mode==='hidden') setInterval(async()=>{{const count=await(await fetch('/version')).json();
 if(count>0&&!changed){{changed=true;if(mode==='hidden') parent.postMessage('hide','*');
 else document.querySelector('img').src='/tile/changed-1';}} }},40);
addEventListener('message',event=>{{if(event.data==='show'&&mode==='motion'){{
 document.querySelector('table').animate([{{transform:'translateX(70px)'}},{{transform:'translateX(0px)'}}],{{duration:900,easing:'linear'}});
 setTimeout(()=>fetch('/motion-finished',{{method:'POST'}}),900);
}}}});
</script></html>"#
    ))
}

async fn tile(Path(id): Path<String>) -> impl IntoResponse {
    let color = if id.starts_with("initial") {
        [210, 40, 40]
    } else {
        [40, 90, 210]
    };
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(RgbImage::from_pixel(100, 100, Rgb(color)))
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    ([("content-type", "image/png")], bytes.into_inner())
}

async fn version(State(state): State<Arc<MockState>>) -> axum::Json<usize> {
    axum::Json(state.calls.load(Ordering::SeqCst))
}

async fn click(State(state): State<Arc<MockState>>, Path(index): Path<usize>) {
    state.clicks.lock().unwrap().push(index);
}

async fn motion_finished(State(state): State<Arc<MockState>>) {
    state.animation_finished.store(true, Ordering::SeqCst);
}
