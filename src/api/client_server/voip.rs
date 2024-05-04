use std::time::{Duration, SystemTime};

use base64::{engine::general_purpose, Engine as _};
use hmac::{Hmac, Mac};
use ruma::{api::client::voip::get_turn_server_info, SecondsSinceUnixEpoch};
use sha1::Sha1;

use crate::{services, Result, Ruma};

type HmacSha1 = Hmac<Sha1>;

/// # `GET /_matrix/client/r0/voip/turnServer`
///
/// TODO: Returns information about the recommended turn server.
pub(crate) async fn turn_server_route(
	body: Ruma<get_turn_server_info::v3::Request>,
) -> Result<get_turn_server_info::v3::Response> {
	let sender_user = body.sender_user.as_ref().expect("user is authenticated");

	let turn_secret = services().globals.turn_secret().clone();

	let (username, password) = if !turn_secret.is_empty() {
		let expiry = SecondsSinceUnixEpoch::from_system_time(
			SystemTime::now()
				.checked_add(Duration::from_secs(services().globals.turn_ttl()))
				.expect("TURN TTL should not get this high"),
		)
		.expect("time is valid");

		let username: String = format!("{}:{}", expiry.get(), sender_user);

		let mut mac = HmacSha1::new_from_slice(turn_secret.as_bytes()).expect("HMAC can take key of any size");
		mac.update(username.as_bytes());

		let password: String = general_purpose::STANDARD.encode(mac.finalize().into_bytes());

		(username, password)
	} else {
		(
			services().globals.turn_username().clone(),
			services().globals.turn_password().clone(),
		)
	};

	Ok(get_turn_server_info::v3::Response {
		username,
		password,
		uris: services().globals.turn_uris().to_vec(),
		ttl: Duration::from_secs(services().globals.turn_ttl()),
	})
}
