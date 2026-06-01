use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::{FromRow, PgPool};
use wasip3::sockets::types::{IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, TcpSocket};

wasip3::cli::command::export!(Component);

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                if let Err(err) = service_main().await {
                    eprintln!("service-pg failed: {err:#}");
                    Err(())
                } else {
                    Ok(())
                }
            })
            .await
    }
}

// ---------------------------------------------------------------------------
// Protocol types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Command {
    /// List todos. When `tag` is set, only todos that carry that tag are returned.
    List {
        #[serde(default)]
        tag: Option<String>,
    },
    /// Create a todo, optionally attaching one or more tags atomically.
    Create {
        description: String,
        #[serde(default)]
        tags: Vec<String>,
    },
    Done {
        id: i64,
    },
    Delete {
        id: i64,
    },
    /// Return every tag with the number of todos referencing it.
    Tags,
}

/// DB row shape for the list query. Maps onto the JOIN result, not a single
/// table: `tags` is the `string_agg` of attached tag names (NULL when empty).
#[derive(FromRow)]
struct TodoRow {
    id: i64,
    description: String,
    done: bool,
    tags: Option<String>,
}

/// API shape. Same fields as [`TodoRow`] but with tags split into a
/// `Vec<String>`, suitable for JSON serialization.
#[derive(Serialize)]
struct Todo {
    id: i64,
    description: String,
    done: bool,
    tags: Vec<String>,
}

impl From<TodoRow> for Todo {
    fn from(r: TodoRow) -> Self {
        let tags = match r.tags {
            Some(s) if !s.is_empty() => s.split(',').map(str::to_string).collect(),
            _ => Vec::new(),
        };
        Self {
            id: r.id,
            description: r.description,
            done: r.done,
            tags,
        }
    }
}

/// Tag with usage count, returned by [`Command::Tags`].
#[derive(FromRow, Serialize)]
struct TagCount {
    name: String,
    count: i64,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Reply {
    List { ok: bool, todos: Vec<Todo> },
    Created { ok: bool, id: i64 },
    Tags { ok: bool, tags: Vec<TagCount> },
    Ack { ok: bool },
    Err { ok: bool, error: String },
}

// ---------------------------------------------------------------------------
// Pool initialisation
// ---------------------------------------------------------------------------

// Switch to `sslmode=require` when testing SQLx over wasi-tls.
const DATABASE_URL: &str = "postgres://postgres:Password123!@127.0.0.1:5432/todos?sslmode=disable";

async fn init_pool() -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy(DATABASE_URL)
        .context("building Postgres pool")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS wasi_todos (
            id BIGSERIAL PRIMARY KEY,
            description TEXT NOT NULL,
            done BOOL NOT NULL DEFAULT FALSE
        )
        "#,
    )
    .execute(&pool)
    .await
    .context("creating wasi_todos table")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS wasi_tags (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(64) NOT NULL UNIQUE
        )
        "#,
    )
    .execute(&pool)
    .await
    .context("creating wasi_tags table")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS wasi_todo_tags (
            todo_id BIGINT NOT NULL,
            tag_id  BIGINT NOT NULL,
            PRIMARY KEY (todo_id, tag_id),
            FOREIGN KEY (todo_id) REFERENCES wasi_todos(id) ON DELETE CASCADE,
            FOREIGN KEY (tag_id)  REFERENCES wasi_tags(id)  ON DELETE CASCADE
        )
        "#,
    )
    .execute(&pool)
    .await
    .context("creating wasi_todo_tags table")?;

    Ok(pool)
}

// ---------------------------------------------------------------------------
// SQL handlers
// ---------------------------------------------------------------------------

/// Unfiltered list: every todo, joined to its tags via `string_agg`.
/// Tag strings come back comma-separated and alphabetized; todos with no
/// tags surface as `NULL` in the `tags` column (handled in [`Todo::from`]).
const LIST_ALL_SQL: &str = r#"
    SELECT t.id, t.description, t.done,
           string_agg(tg.name, ',' ORDER BY tg.name) AS tags
    FROM wasi_todos t
    LEFT JOIN wasi_todo_tags tt ON tt.todo_id = t.id
    LEFT JOIN wasi_tags      tg ON tg.id      = tt.tag_id
    GROUP BY t.id, t.description, t.done
    ORDER BY t.id
"#;

/// Tag-filtered list. The `EXISTS` subquery restricts which todos appear,
/// but the outer JOIN still surfaces *all* of each surviving todo's tags.
const LIST_BY_TAG_SQL: &str = r#"
    SELECT t.id, t.description, t.done,
           string_agg(tg.name, ',' ORDER BY tg.name) AS tags
    FROM wasi_todos t
    LEFT JOIN wasi_todo_tags tt ON tt.todo_id = t.id
    LEFT JOIN wasi_tags      tg ON tg.id      = tt.tag_id
    WHERE EXISTS (
        SELECT 1 FROM wasi_todo_tags tt2
        JOIN wasi_tags tg2 ON tg2.id = tt2.tag_id
        WHERE tt2.todo_id = t.id AND tg2.name = $1
    )
    GROUP BY t.id, t.description, t.done
    ORDER BY t.id
"#;

async fn handle_command(pool: &PgPool, cmd: Command) -> Result<Reply> {
    match cmd {
        Command::List { tag } => {
            // Two literal SQL strings rather than `format!` — the sqlx fork
            // requires `&'static str` here (which doubles as a static guard
            // against accidentally interpolating user input into SQL).
            let rows: Vec<TodoRow> = if let Some(filter) = tag.as_deref() {
                sqlx::query_as::<_, TodoRow>(LIST_BY_TAG_SQL)
                    .bind(filter)
                    .fetch_all(pool)
                    .await?
            } else {
                sqlx::query_as::<_, TodoRow>(LIST_ALL_SQL)
                    .fetch_all(pool)
                    .await?
            };
            let todos = rows.into_iter().map(Todo::from).collect();
            Ok(Reply::List { ok: true, todos })
        }
        Command::Create { description, tags } => {
            // Single transaction: insert the todo, upsert each tag, link them.
            // Roll back if any step fails so we never end up with a half-tagged
            // row.
            let mut tx = pool.begin().await?;

            let todo_id: i64 =
                sqlx::query_scalar("INSERT INTO wasi_todos (description) VALUES ($1) RETURNING id")
                    .bind(&description)
                    .fetch_one(&mut *tx)
                    .await?;

            for tag in tags.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
                sqlx::query(
                    "INSERT INTO wasi_tags (name) VALUES ($1) ON CONFLICT (name) DO NOTHING",
                )
                .bind(tag)
                .execute(&mut *tx)
                .await?;

                let tag_id: i64 = sqlx::query_scalar("SELECT id FROM wasi_tags WHERE name = $1")
                    .bind(tag)
                    .fetch_one(&mut *tx)
                    .await?;

                sqlx::query(
                    r#"
                    INSERT INTO wasi_todo_tags (todo_id, tag_id)
                    VALUES ($1, $2)
                    ON CONFLICT (todo_id, tag_id) DO NOTHING
                    "#,
                )
                .bind(todo_id)
                .bind(tag_id)
                .execute(&mut *tx)
                .await?;
            }

            tx.commit().await?;
            Ok(Reply::Created {
                ok: true,
                id: todo_id,
            })
        }
        Command::Done { id } => {
            sqlx::query("UPDATE wasi_todos SET done = TRUE WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await?;
            Ok(Reply::Ack { ok: true })
        }
        Command::Delete { id } => {
            // ON DELETE CASCADE on wasi_todo_tags handles the link rows. Tags
            // themselves stick around so the chip cloud stays stable across
            // single-todo deletes.
            sqlx::query("DELETE FROM wasi_todos WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await?;
            Ok(Reply::Ack { ok: true })
        }
        Command::Tags => {
            let tags = sqlx::query_as::<_, TagCount>(
                r#"
                SELECT tg.name AS name, COUNT(tt.todo_id)::BIGINT AS count
                FROM wasi_tags tg
                LEFT JOIN wasi_todo_tags tt ON tt.tag_id = tg.id
                GROUP BY tg.id, tg.name
                ORDER BY tg.name
                "#,
            )
            .fetch_all(pool)
            .await?;
            Ok(Reply::Tags { ok: true, tags })
        }
    }
}

// ---------------------------------------------------------------------------
// TCP server
// ---------------------------------------------------------------------------

async fn service_main() -> Result<()> {
    let pool = Arc::new(init_pool().await?);

    let listener = TcpSocket::create(IpAddressFamily::Ipv4)
        .map_err(|e| anyhow::anyhow!("TcpSocket::create: {e:?}"))?;

    listener
        .bind(IpSocketAddress::Ipv4(Ipv4SocketAddress {
            port: 7777,
            address: (0, 0, 0, 0),
        }))
        .map_err(|e| anyhow::anyhow!("bind: {e:?}"))?;

    let mut incoming = listener
        .listen()
        .map_err(|e| anyhow::anyhow!("listen: {e:?}"))?;

    eprintln!("service-pg: pool ready, listening on 0.0.0.0:7777");

    loop {
        let client = match incoming.next().await {
            Some(c) => c,
            None => break,
        };
        let pool = Arc::clone(&pool);
        tokio::task::spawn_local(async move {
            if let Err(err) = serve_conn(client, pool).await {
                eprintln!("connection error: {err:#}");
            }
        });
    }

    Ok(())
}

async fn serve_conn(socket: TcpSocket, pool: Arc<PgPool>) -> Result<()> {
    // StreamReader<u8> has `async fn next(&mut self) -> Option<u8>` — not futures::Stream.
    let (mut rx, _done_fut) = socket.receive();
    let mut line_buf: Vec<u8> = Vec::new();

    loop {
        let byte = match rx.next().await {
            Some(b) => b,
            None => return Ok(()), // EOF
        };

        if byte == b'\n' {
            let reply = match serde_json::from_slice::<Command>(&line_buf) {
                Ok(cmd) => handle_command(&pool, cmd)
                    .await
                    .unwrap_or_else(|e| Reply::Err {
                        ok: false,
                        error: format!("{e:#}"),
                    }),
                Err(e) => Reply::Err {
                    ok: false,
                    error: format!("invalid command: {e}"),
                },
            };

            let mut out = serde_json::to_vec(&reply)?;
            out.push(b'\n');

            let (mut writer, reader) = wasip3::wit_stream::new::<u8>();
            let send_fut = socket.send(reader);
            writer.write_all(out).await;
            drop(writer);
            send_fut
                .await
                .map_err(|e| anyhow::anyhow!("send error: {e:?}"))?;

            line_buf.clear();
        } else {
            line_buf.push(byte);
        }
    }
}
