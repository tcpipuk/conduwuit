use std::{convert::TryFrom, convert::TryInto, sync::Arc, time::Instant};

use crate::{
    error::{Error, Result},
    pdu::PduBuilder,
    server_server, Database, PduEvent,
};
use clap::Parser;
use regex::Regex;
use rocket::{
    futures::{channel::mpsc, stream::StreamExt},
    http::RawStr,
};
use ruma::{
    events::{room::message::RoomMessageEventContent, EventType},
    EventId, RoomId, RoomVersionId, UserId,
};
use serde_json::value::to_raw_value;
use tokio::sync::{MutexGuard, RwLock, RwLockReadGuard};
use tracing::warn;

pub enum AdminRoomEvent {
    ProcessMessage(String),
    SendMessage(RoomMessageEventContent),
}

#[derive(Clone)]
pub struct Admin {
    pub sender: mpsc::UnboundedSender<AdminRoomEvent>,
}

impl Admin {
    pub fn start_handler(
        &self,
        db: Arc<RwLock<Database>>,
        mut receiver: mpsc::UnboundedReceiver<AdminRoomEvent>,
    ) {
        tokio::spawn(async move {
            // TODO: Use futures when we have long admin commands
            //let mut futures = FuturesUnordered::new();

            let guard = db.read().await;

            let conduit_user = UserId::parse(format!("@conduit:{}", guard.globals.server_name()))
                .expect("@conduit:server_name is valid");

            let conduit_room = guard
                .rooms
                .id_from_alias(
                    format!("#admins:{}", guard.globals.server_name())
                        .as_str()
                        .try_into()
                        .expect("#admins:server_name is a valid room alias"),
                )
                .expect("Admin room must exist");

            let conduit_room = match conduit_room {
                None => {
                    warn!("Conduit instance does not have an #admins room. Logging to that room will not work. Restart Conduit after creating a user to fix this.");
                    return;
                }
                Some(r) => r,
            };

            drop(guard);

            let send_message = |message: RoomMessageEventContent,
                                guard: RwLockReadGuard<'_, Database>,
                                mutex_lock: &MutexGuard<'_, ()>| {
                guard
                    .rooms
                    .build_and_append_pdu(
                        PduBuilder {
                            event_type: EventType::RoomMessage,
                            content: to_raw_value(&message)
                                .expect("event is valid, we just created it"),
                            unsigned: None,
                            state_key: None,
                            redacts: None,
                        },
                        &conduit_user,
                        &conduit_room,
                        &guard,
                        mutex_lock,
                    )
                    .unwrap();
            };

            loop {
                tokio::select! {
                    Some(event) = receiver.next() => {
                        let guard = db.read().await;
                        let mutex_state = Arc::clone(
                            guard.globals
                                .roomid_mutex_state
                                .write()
                                .unwrap()
                                .entry(conduit_room.clone())
                                .or_default(),
                        );
                        let state_lock = mutex_state.lock().await;

                        match event {
                            AdminRoomEvent::SendMessage(content) => {
                                send_message(content, guard, &state_lock);
                            }
                            AdminRoomEvent::ProcessMessage(room_message) => {
                                let reply_message = process_admin_message(&*guard, room_message);

                                send_message(reply_message, guard, &state_lock);
                            }
                        }

                        drop(state_lock);
                    }
                }
            }
        });
    }

    pub fn process_message(&self, room_message: String) {
        self.sender
            .unbounded_send(AdminRoomEvent::ProcessMessage(room_message))
            .unwrap();
    }

    pub fn send_message(&self, message_content: RoomMessageEventContent) {
        self.sender
            .unbounded_send(AdminRoomEvent::SendMessage(message_content))
            .unwrap();
    }
}

// Parse and process a message from the admin room
pub fn process_admin_message(db: &Database, room_message: String) -> RoomMessageEventContent {
    let mut lines = room_message.lines();
    let command_line = lines.next().expect("each string has at least one line");
    let body: Vec<_> = lines.collect();

    let admin_command = match parse_admin_command(&command_line) {
        Ok(command) => command,
        Err(error) => {
            let message = error
                .to_string()
                .replace("example.com", db.globals.server_name().as_str());
            let html_message = usage_to_html(&message);

            return RoomMessageEventContent::text_html(message, html_message);
        }
    };

    match process_admin_command(db, admin_command, body) {
        Ok(reply_message) => reply_message,
        Err(error) => {
            let markdown_message = format!(
                "Encountered an error while handling the command:\n\
                ```\n{}\n```",
                error,
            );
            let html_message = format!(
                "Encountered an error while handling the command:\n\
                <pre>\n{}\n</pre>",
                error,
            );

            RoomMessageEventContent::text_html(markdown_message, html_message)
        }
    }
}

// Parse chat messages from the admin room into an AdminCommand object
fn parse_admin_command(command_line: &str) -> std::result::Result<AdminCommand, String> {
    // Note: argv[0] is `@conduit:servername:`, which is treated as the main command
    let mut argv: Vec<_> = command_line.split_whitespace().collect();

    // Replace `help command` with `command --help`
    // Clap has a help subcommand, but it omits the long help description.
    if argv.len() > 1 && argv[1] == "help" {
        argv.remove(1);
        argv.push("--help");
    }

    // Backwards compatibility with `register_appservice`-style commands
    let command_with_dashes;
    if argv.len() > 1 && argv[1].contains("_") {
        command_with_dashes = argv[1].replace("_", "-");
        argv[1] = &command_with_dashes;
    }

    AdminCommand::try_parse_from(argv).map_err(|error| error.to_string())
}

#[derive(Parser)]
#[clap(name = "@conduit:example.com", version = env!("CARGO_PKG_VERSION"))]
enum AdminCommand {
    #[clap(verbatim_doc_comment)]
    /// Register an appservice using its registration YAML
    ///
    /// This command needs a YAML generated by an appservice (such as a bridge),
    /// which must be provided in a Markdown code-block below the command.
    ///
    /// Registering a new bridge using the ID of an existing bridge will replace
    /// the old one.
    ///
    /// [add-yaml-block-to-usage]
    RegisterAppservice,

    /// Unregister an appservice using its ID
    ///
    /// You can find the ID using the `list-appservices` command.
    UnregisterAppservice {
        /// The appservice to unregister
        appservice_identifier: String,
    },

    /// List all the currently registered appservices
    ListAppservices,

    /// List users in the database
    ListLocalUsers,

    /// Get the auth_chain of a PDU
    GetAuthChain {
        /// An event ID (the $ character followed by the base64 reference hash)
        event_id: Box<EventId>,
    },

    /// Parse and print a PDU from a JSON
    ///
    /// The PDU event is only checked for validity and is not added to the
    /// database.
    ParsePdu,

    /// Retrieve and print a PDU by ID from the Conduit database
    GetPdu {
        /// An event ID (a $ followed by the base64 reference hash)
        event_id: Box<EventId>,
    },

    /// Print database memory usage statistics
    DatabaseMemoryUsage,
}

fn process_admin_command(
    db: &Database,
    command: AdminCommand,
    body: Vec<&str>,
) -> Result<RoomMessageEventContent> {
    let reply_message_content = match command {
        AdminCommand::RegisterAppservice => {
            if body.len() > 2 && body[0].trim() == "```" && body.last().unwrap().trim() == "```" {
                let appservice_config = body[1..body.len() - 1].join("\n");
                let parsed_config = serde_yaml::from_str::<serde_yaml::Value>(&appservice_config);
                match parsed_config {
                    Ok(yaml) => match db.appservice.register_appservice(yaml) {
                        Ok(()) => RoomMessageEventContent::text_plain("Appservice registered."),
                        Err(e) => RoomMessageEventContent::text_plain(format!(
                            "Failed to register appservice: {}",
                            e
                        )),
                    },
                    Err(e) => RoomMessageEventContent::text_plain(format!(
                        "Could not parse appservice config: {}",
                        e
                    )),
                }
            } else {
                RoomMessageEventContent::text_plain(
                    "Expected code block in command body. Add --help for details.",
                )
            }
        }
        AdminCommand::UnregisterAppservice {
            appservice_identifier,
        } => match db.appservice.unregister_appservice(&appservice_identifier) {
            Ok(()) => RoomMessageEventContent::text_plain("Appservice unregistered."),
            Err(e) => RoomMessageEventContent::text_plain(format!(
                "Failed to unregister appservice: {}",
                e
            )),
        },
        AdminCommand::ListAppservices => {
            if let Ok(appservices) = db.appservice.iter_ids().map(|ids| ids.collect::<Vec<_>>()) {
                let count = appservices.len();
                let output = format!(
                    "Appservices ({}): {}",
                    count,
                    appservices
                        .into_iter()
                        .filter_map(|r| r.ok())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                RoomMessageEventContent::text_plain(output)
            } else {
                RoomMessageEventContent::text_plain("Failed to get appservices.")
            }
        }
        AdminCommand::ListLocalUsers => match db.users.list_local_users() {
            Ok(users) => {
                let mut msg: String = format!("Found {} local user account(s):\n", users.len());
                msg += &users.join("\n");
                RoomMessageEventContent::text_plain(&msg)
            }
            Err(e) => RoomMessageEventContent::text_plain(e.to_string()),
        },
        AdminCommand::GetAuthChain { event_id } => {
            let event_id = Arc::<EventId>::from(event_id);
            if let Some(event) = db.rooms.get_pdu_json(&event_id)? {
                let room_id_str = event
                    .get("room_id")
                    .and_then(|val| val.as_str())
                    .ok_or_else(|| Error::bad_database("Invalid event in database"))?;

                let room_id = <&RoomId>::try_from(room_id_str).map_err(|_| {
                    Error::bad_database("Invalid room id field in event in database")
                })?;
                let start = Instant::now();
                let count = server_server::get_auth_chain(room_id, vec![event_id], db)?.count();
                let elapsed = start.elapsed();
                RoomMessageEventContent::text_plain(format!(
                    "Loaded auth chain with length {} in {:?}",
                    count, elapsed
                ))
            } else {
                RoomMessageEventContent::text_plain("Event not found.")
            }
        }
        AdminCommand::ParsePdu => {
            if body.len() > 2 && body[0].trim() == "```" && body.last().unwrap().trim() == "```" {
                let string = body[1..body.len() - 1].join("\n");
                match serde_json::from_str(&string) {
                    Ok(value) => {
                        let event_id = EventId::parse(format!(
                            "${}",
                            // Anything higher than version3 behaves the same
                            ruma::signatures::reference_hash(&value, &RoomVersionId::V6)
                                .expect("ruma can calculate reference hashes")
                        ))
                        .expect("ruma's reference hashes are valid event ids");

                        match serde_json::from_value::<PduEvent>(
                            serde_json::to_value(value).expect("value is json"),
                        ) {
                            Ok(pdu) => RoomMessageEventContent::text_plain(format!(
                                "EventId: {:?}\n{:#?}",
                                event_id, pdu
                            )),
                            Err(e) => RoomMessageEventContent::text_plain(format!(
                                "EventId: {:?}\nCould not parse event: {}",
                                event_id, e
                            )),
                        }
                    }
                    Err(e) => RoomMessageEventContent::text_plain(format!(
                        "Invalid json in command body: {}",
                        e
                    )),
                }
            } else {
                RoomMessageEventContent::text_plain("Expected code block in command body.")
            }
        }
        AdminCommand::GetPdu { event_id } => {
            let mut outlier = false;
            let mut pdu_json = db.rooms.get_non_outlier_pdu_json(&event_id)?;
            if pdu_json.is_none() {
                outlier = true;
                pdu_json = db.rooms.get_pdu_json(&event_id)?;
            }
            match pdu_json {
                Some(json) => {
                    let json_text =
                        serde_json::to_string_pretty(&json).expect("canonical json is valid json");
                    RoomMessageEventContent::text_html(
                        format!(
                            "{}\n```json\n{}\n```",
                            if outlier {
                                "PDU is outlier"
                            } else {
                                "PDU was accepted"
                            },
                            json_text
                        ),
                        format!(
                            "<p>{}</p>\n<pre><code class=\"language-json\">{}\n</code></pre>\n",
                            if outlier {
                                "PDU is outlier"
                            } else {
                                "PDU was accepted"
                            },
                            RawStr::new(&json_text).html_escape()
                        ),
                    )
                }
                None => RoomMessageEventContent::text_plain("PDU not found."),
            }
        }
        AdminCommand::DatabaseMemoryUsage => match db._db.memory_usage() {
            Ok(response) => RoomMessageEventContent::text_plain(response),
            Err(e) => RoomMessageEventContent::text_plain(format!(
                "Failed to get database memory usage: {}",
                e
            )),
        },
    };

    Ok(reply_message_content)
}

// Utility to turn clap's `--help` text to HTML.
fn usage_to_html(text: &str) -> String {
    // For the conduit admin room, subcommands become main commands
    let text = text.replace("SUBCOMMAND", "COMMAND");
    let text = text.replace("subcommand", "command");

    // Escape option names (e.g. `<element-id>`) since they look like HTML tags
    let text = text.replace("<", "&lt;").replace(">", "&gt;");

    // Italicize the first line (command name and version text)
    let re = Regex::new("^(.*?)\n").expect("Regex compilation should not fail");
    let text = re.replace_all(&text, "<em>$1</em>\n");

    // Unmerge wrapped lines
    let text = text.replace("\n            ", "  ");

    // Wrap option names in backticks. The lines look like:
    //     -V, --version  Prints version information
    // And are converted to:
    // <code>-V, --version</code>: Prints version information
    // (?m) enables multi-line mode for ^ and $
    let re = Regex::new("(?m)^    (([a-zA-Z_&;-]+(, )?)+)  +(.*)$")
        .expect("Regex compilation should not fail");
    let text = re.replace_all(&text, "<code>$1</code>: $4");

    // // Enclose examples in code blocks
    // // (?ms) enables multi-line mode and dot-matches-all
    // let re =
    //     Regex::new("(?ms)^Example:\n(.*?)\nUSAGE:$").expect("Regex compilation should not fail");
    // let text = re.replace_all(&text, "EXAMPLE:\n<pre>$1</pre>\nUSAGE:");

    let has_yaml_block_marker = text.contains("\n[add-yaml-block-to-usage]\n");
    let text = text.replace("\n[add-yaml-block-to-usage]\n", "");

    // Add HTML line-breaks
    let text = text.replace("\n", "<br>\n");

    let text = if !has_yaml_block_marker {
        // Wrap the usage line in code tags
        let re = Regex::new("(?m)^USAGE:<br>\n    (@conduit:.*)<br>$")
            .expect("Regex compilation should not fail");
        re.replace_all(&text, "USAGE:<br>\n<code>$1</code><br>")
    } else {
        // Wrap the usage line in a code block, and add a yaml block example
        // This makes the usage of e.g. `register-appservice` more accurate
        let re = Regex::new("(?m)^USAGE:<br>\n    (.*?)<br>\n<br>\n")
            .expect("Regex compilation should not fail");
        re.replace_all(
            &text,
            "USAGE:<br>\n<pre>$1\n```\nyaml content here\n```</pre>",
        )
    };

    text.to_string()
}
