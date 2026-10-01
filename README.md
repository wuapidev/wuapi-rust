<!-- Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/README.md. Do not edit here. -->

# wuapi Rust SDK

[![crates.io](https://img.shields.io/crates/v/wuapi.svg)](https://crates.io/crates/wuapi)
[![docs.rs](https://docs.rs/wuapi/badge.svg)](https://docs.rs/wuapi)
![license](https://img.shields.io/crates/l/wuapi.svg)

Rust SDK for [wuapi](https://wuapi.dev), a WhatsApp API for developers. Link your own WhatsApp numbers by QR code or pairing code, send and receive messages, manage chats, contacts, groups, communities and channels, split them into projects, and verify webhooks.

- Async, on `reqwest` and `tokio`. TLS is rustls: there is no OpenSSL to install.
- Types for every schema of the [OpenAPI spec](https://wuapi.dev/openapi.json), with `serde`.
- A timeout on every attempt, retries that never repeat a send, and idempotency keys built in.

Docs: [wuapi.dev/docs](https://wuapi.dev/docs). API reference for this crate: [docs.rs/wuapi](https://docs.rs/wuapi).

> **How it works.** wuapi does not use the WhatsApp Business Platform (Cloud API). Numbers are linked as devices, the same way WhatsApp Web works. WhatsApp can restrict or ban numbers that behave like spam. You are responsible for your recipients' consent and for following WhatsApp's terms.

## Install

```sh
cargo add wuapi
cargo add tokio --features macros,rt-multi-thread
```

The crate needs Rust 1.85 or later and a `tokio` runtime.

## Authentication

Create an API key in the dashboard at [wuapi.dev/app/api-keys](https://wuapi.dev/app/api-keys). Keys look like `wu_live_...` and are sent as `Authorization: Bearer <key>`. Keep them on your server.

```rust,no_run
use wuapi::Wuapi;

fn clients() -> Result<(Wuapi, Wuapi), wuapi::Error> {
    let from_env = Wuapi::from_env()?; // reads WUAPI_API_KEY
    let explicit = Wuapi::new("wu_live_...")?;
    Ok((from_env, explicit))
}
```

A missing key is an `Error::Config` when the client is built. The client is cheap to clone: clones share one connection pool.

An organization key reaches every project. A project key, created with `projects().api_keys().create(..)`, reaches only its project: see [Projects](#projects).

## Quickstart: link a number and send a message

```rust,no_run
use std::time::Duration;

use wuapi::types::{AccountCreateRequest, AccountStatus, ProxyLocationInput, SendTextMessageRequest};
use wuapi::Wuapi;

#[tokio::main]
async fn main() -> Result<(), wuapi::Error> {
    let client = Wuapi::from_env()?;

    // 1. Create an account. Its traffic exits through a residential proxy in
    //    the country and city you name: use the phone number's country.
    //    `proxy_locations().list(..)` returns every supported pair.
    let mut request = AccountCreateRequest::new(ProxyLocationInput::new("VE", "caracas"));
    request.name = Some("Support line".to_owned());
    let account = client.accounts().create(request).await?;

    // 2. Poll the account: show the QR code to the phone owner (WhatsApp >
    //    Linked devices > Link a device) and wait until the phone has linked.
    //    The QR code rotates while you wait.
    let ready = loop {
        let account = client.accounts().get(&account.id).await?;
        match account.status {
            AccountStatus::Ready => break account,
            AccountStatus::QrReady => println!("Scan this QR code: {:?}", account.qr_code_url),
            AccountStatus::Failed => panic!("linking failed: {:?}", account.last_error),
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    // 3. Send a message.
    let text = SendTextMessageRequest::new(&ready.id, "+584241112233", "Your order has shipped.");
    let message = client.messages().send(text).await?;
    println!("{} {}", message.id, message.status); // "queued"
    Ok(())
}
```

A send returns the message with `status` `queued`. The outcome arrives as the `message.sent` or `message.failed` webhook, or by calling `client.messages().get(id)`.

To link by phone number instead of a QR code, set `pairing_phone` on the create request, or call `accounts().create_pairing_code(..)`, and show `account.pairing_code` to the phone owner.

Contacts are E.164 (`+584241112233`), or `lid:<digits>` when WhatsApp hides the number; groups are `...@g.us`, channels `...@newsletter`.

## Sending other types

`messages().send(..)` takes any of the `Send...MessageRequest` types. Each has a `new` with its required fields; the optional ones are public fields that start as `None`.

```rust,no_run
use wuapi::types::{SendDocumentMessageRequest, SendMedia, SendTextMessageRequest};
use wuapi::Wuapi;

async fn examples(client: &Wuapi, account_id: &str) -> Result<(), wuapi::Error> {
    let mut media = SendMedia::new("https://example.com/invoice.pdf");
    media.filename = Some("invoice.pdf".to_owned());
    let mut document = SendDocumentMessageRequest::new(account_id, "+584241112233", media);
    document.text = Some("Your invoice".to_owned());
    client.messages().send(document).await?;

    // Reply to (quote) a message in the same chat.
    let mut reply = SendTextMessageRequest::new(account_id, "+584241112233", "Tomorrow.");
    reply.reply_to_message_id = Some("msg_...".to_owned());
    let sent = client.messages().send(reply).await?;

    // Star what you sent, then delete it.
    client.messages().star(&sent.id).await?;
    client.messages().delete(&sent.id, Default::default()).await?;
    Ok(())
}
```

Everything else works on the account: chats, contacts, the profile, privacy, stories, groups and communities, channels, labels and calls. Those methods take the account id first, and the account must be `ready`. The methods are grouped and named like the [TypeScript SDK](https://www.npmjs.com/package/@wuapidev/sdk)'s, in snake case: `client.chats().archive(account_id, chat_id)`, `client.groups().add_participants(account_id, group_id, params)`.

## Drops, timeouts and retries

Every number connects through a residential proxy, so a request can be slow or lose its connection halfway. The client is built for that:

- **Every attempt has a timeout**: 30 seconds by default, per attempt, from connecting until the body has been read. Change it for the client with `builder().timeout(..)` or for one call with `.timeout(..)`.
- **A send carries an idempotency key.** Every method whose endpoint accepts `Idempotency-Key` (sends, reactions, group changes, creates) sends a random key, and every retry of that call sends the same one, so the API replays the first answer instead of sending twice.
- **Retries only where they cannot repeat work.** `GET`, `PUT` and `DELETE` requests and requests with an idempotency key are retried on timeouts, network errors, `429` and `5xx`, with exponential backoff and jitter (2 retries by default). A request that is not idempotent (the `PATCH` updates) is retried only when it never reached the API (the connection could not be opened) or was refused with `429`. A `Retry-After` header is honored; one longer than 60 seconds fails at once with the wait in `Error::retry_after()`.

The random key covers the retries the client makes on its own. To make a request you send again later the same request (after your process restarted, or from a job queue), give it your own key:

```rust,no_run
use std::time::Duration;

use wuapi::types::SendTextMessageRequest;
use wuapi::Wuapi;

async fn notify(client: &Wuapi, order_id: u64, text: SendTextMessageRequest) -> Result<(), wuapi::Error> {
    client
        .messages()
        .send(text)
        .idempotency_key(format!("order-{order_id}-shipped"))
        .timeout(Duration::from_secs(10))
        .await?;
    Ok(())
}
```

The same key with the same request answers with the first response for 24 hours. A key reused with a different body answers `409 idempotency_conflict`.

A request does nothing until it is awaited. Dropping its future cancels it.

## Lists and pagination

List methods return a `Paginator`. Stream it to walk every item across pages, or ask for one page:

```rust,no_run
use wuapi::pagination::TryStreamExt;
use wuapi::types::{MessageDirection, MessagesListParams};
use wuapi::Wuapi;

async fn inbound(client: &Wuapi, account_id: &str) -> Result<(), wuapi::Error> {
    let params = MessagesListParams {
        account_id: Some(account_id.to_owned()),
        direction: Some(MessageDirection::Inbound),
        ..Default::default()
    };

    let mut messages = client.messages().list(params.clone()).stream();
    while let Some(message) = messages.try_next().await? {
        println!("{:?} {:?}", message.from, message.text);
    }

    let page = client.messages().list(params.clone()).page().await?;
    if let Some(cursor) = &page.next_cursor {
        let next = client.messages().list(params).page_at(cursor).await?;
        println!("{} more", next.items.len());
    }
    Ok(())
}
```

`pages()` streams whole pages, `to_vec()` collects everything and `to_vec_max(n)` stops after `n` items. Every list has the same shape, `object`, `items` and `next_cursor`, including lists read live from WhatsApp.

## Webhooks

Create an endpoint once and store the secret; it is returned only on creation.

```rust,no_run
use wuapi::types::{WebhookEndpointCreateRequest, WebhookEventType};
use wuapi::Wuapi;

async fn subscribe(client: &Wuapi) -> Result<(), wuapi::Error> {
    let events = vec![WebhookEventType::MessageReceived, WebhookEventType::AccountDisconnected];
    let request = WebhookEndpointCreateRequest::new("https://example.com/webhooks/wuapi", events);
    let endpoint = client.webhook_endpoints().create(request).await?;
    println!("{:?}", endpoint.secret); // whsec_...
    Ok(())
}
```

Verify each request with the raw body, before parsing it as JSON:

```rust,no_run
use wuapi::types::Event;
use wuapi::{WebhookVerificationError, verify_webhook};

/// `signature` is the `Wuapi-Signature` header.
fn handle(raw_body: &[u8], signature: &str, secret: &str) -> Result<(), WebhookVerificationError> {
    match verify_webhook(raw_body, signature, secret)? {
        Event::Message(event) => println!("{:?} {:?}", event.r#type, event.data.object.text),
        Event::Account(event) => println!("{} is {}", event.data.object.id, event.data.object.status),
        Event::Unknown(raw) => println!("an event newer than this SDK: {raw}"),
        _ => {}
    }
    Ok(())
}
```

`verify_webhook` checks the `Wuapi-Signature` header (`t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<rawBody>">`) in constant time and rejects timestamps more than 300 seconds away; `verify_webhook_with_tolerance` takes another tolerance, and `verify_signature` checks without parsing. Answer anything but 2xx when it fails. Deliveries can repeat: deduplicate on the event's `id`.

An event type added after this version of the SDK parses as `Event::Unknown` with its raw JSON, instead of failing.

## Projects

Three levels: your **organization** pays; a **project** is one of your customers, or an environment, with its own accounts, API keys, webhooks, limits and usage; an **account** is a linked WhatsApp number.

```rust,no_run
use wuapi::types::{AccountsListParams, ProjectCreateRequest};
use wuapi::Wuapi;

async fn projects(client: &Wuapi) -> Result<(), wuapi::Error> {
    // Organization key: create the project with your own id for the customer.
    let mut request = ProjectCreateRequest::new("Northwind Dental");
    request.external_id = Some("customer_8812".to_owned());
    let project = client.projects().create(request).await?;

    // Act inside it: every request carries the Wuapi-Project header.
    let northwind = client.with_project("ext:customer_8812")?; // or &project.id
    let accounts = northwind.accounts().list(AccountsListParams::default()).to_vec().await?;
    println!("{} has {} accounts", project.id, accounts.len());
    Ok(())
}
```

`Wuapi::builder().project(..)` does the same from the start. A resource outside the scope answers `404 not_found`; a project key naming another project answers `403 forbidden`.

## Errors

Every failure is a `wuapi::Error`:

| Variant | When | `code()` |
|---|---|---|
| `Error::Api` | The API answered with a non-2xx status. Carries `status`, `code`, `message`, `details`, `request_id` and `retry_after`. | the API's code, such as `account_not_ready` |
| `Error::Timeout` | An attempt ran out of time and no retry was left or allowed. | `timeout` |
| `Error::Network` | The request could not be sent or the response could not be read. | `network_error` |
| `Error::Decode` | A 2xx response did not match the expected type. | `invalid_response` |
| `Error::Encode` | The request body could not be serialized. Nothing was sent. | `invalid_request` |
| `Error::Config` | No API key, or a bad project. Nothing was sent. | `configuration` |

```rust,no_run
use wuapi::types::SendTextMessageRequest;
use wuapi::{Error, Wuapi};

async fn send(client: &Wuapi, text: SendTextMessageRequest) {
    match client.messages().send(text).await {
        Ok(message) => println!("queued {}", message.id),
        Err(Error::Api { code, retry_after, .. }) if code == "account_not_ready" => {
            println!("the number is offline; retry after {retry_after:?}");
        }
        Err(error) => eprintln!(
            "{error} (status {:?}, request {:?})",
            error.status(),
            error.request_id()
        ),
    }
}
```

`request_id` is the `x-request-id` response header. Quote it when you contact support.

## Configuration

```rust,no_run
use std::time::Duration;

use wuapi::{RetryPolicy, Wuapi};

fn client() -> Result<Wuapi, wuapi::Error> {
    Wuapi::builder()
        .api_key("wu_live_...")
        .base_url("https://api.wuapi.dev")
        .timeout(Duration::from_secs(20))
        .retry(RetryPolicy {
            max_retries: 4,
            ..RetryPolicy::default()
        })
        .project("ext:customer_8812")
        .build()
}
```

`http_client(..)` takes your own `reqwest::Client`, for a proxy or a shared connection pool. `RetryPolicy::none()` turns retries off.

An endpoint this version does not cover yet can be called through the same client, with the same auth, timeout and retry rules:

```rust,no_run
use wuapi::{Method, RequestParts, Wuapi};

async fn raw(client: &Wuapi) -> Result<serde_json::Value, wuapi::Error> {
    let parts = RequestParts::new(Method::Get, "/v1/me").retryable();
    client.http().request(parts).await
}
```

## Types

`wuapi::types` holds one type per schema of the spec, and the params of every method.

- Optional fields are `Option`s and are left out of the request when `None`. A field that may be absent or `null` is an `Option<Option<T>>`: `None` leaves it out, `Some(None)` sends `null` (as in `pacing: null`, which resets an account's pacing).
- Enums keep a value this version does not know in `Unknown(String)`, and unions keep an unknown shape in `Unknown(serde_json::Value)`, so a value the API adds later still parses.
- Timestamps are ISO 8601 strings, as the API sends them.

## Generated code

This crate, version 0.5.0, is generated from the wuapi OpenAPI spec: its types, methods, tests and this README. Do not edit it by hand. Report problems at [wuapi.dev/support](https://wuapi.dev/support).

## License

MIT
