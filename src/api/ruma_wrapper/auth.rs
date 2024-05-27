use std::collections::BTreeMap;

use axum::RequestPartsExt;
use axum_extra::{headers::Authorization, typed_header::TypedHeaderRejectionReason, TypedHeader};
use http::uri::PathAndQuery;
use ruma::{
	api::{client::error::ErrorKind, AuthScheme, IncomingRequest},
	CanonicalJsonValue, OwnedDeviceId, OwnedServerName, OwnedUserId, UserId,
};
use tracing::warn;

use super::{request::Request, xmatrix::XMatrix};
use crate::{service::appservice::RegistrationInfo, services, Error, Result};

enum Token {
	Appservice(Box<RegistrationInfo>),
	User((OwnedUserId, OwnedDeviceId)),
	Invalid,
	None,
}

pub(super) struct Auth {
	pub(super) sender_user: Option<OwnedUserId>,
	pub(super) sender_device: Option<OwnedDeviceId>,
	pub(super) origin: Option<OwnedServerName>,
	pub(super) appservice_info: Option<RegistrationInfo>,
}

pub(super) async fn auth<T>(request: &mut Request) -> Result<Auth>
where
	T: IncomingRequest,
{
	let metadata = T::METADATA;
	let token = match &request.auth {
		Some(TypedHeader(Authorization(bearer))) => Some(bearer.token()),
		None => request.query.access_token.as_deref(),
	};

	let token = if let Some(token) = token {
		if let Some(reg_info) = services().appservice.find_from_token(token).await {
			Token::Appservice(Box::new(reg_info))
		} else if let Some((user_id, device_id)) = services().users.find_from_token(token)? {
			Token::User((user_id, OwnedDeviceId::from(device_id)))
		} else {
			Token::Invalid
		}
	} else {
		Token::None
	};

	if metadata.authentication == AuthScheme::None {
		match request.parts.uri.path() {
			// TODO: can we check this better?
			"/_matrix/client/v3/publicRooms" | "/_matrix/client/r0/publicRooms" => {
				if !services()
					.globals
					.config
					.allow_public_room_directory_without_auth
				{
					match token {
						Token::Appservice(_) | Token::User(_) => {
							// we should have validated the token above
							// already
						},
						Token::None | Token::Invalid => {
							return Err(Error::BadRequest(ErrorKind::MissingToken, "Missing or invalid access token."));
						},
					}
				}
			},
			_ => {},
		};
	}

	match (metadata.authentication, token) {
		(_, Token::Invalid) => Err(Error::BadRequest(
			ErrorKind::UnknownToken {
				soft_logout: false,
			},
			"Unknown access token.",
		)),
		(AuthScheme::AccessToken, Token::Appservice(info)) => Ok(auth_appservice(request, info)?),
		(AuthScheme::None | AuthScheme::AccessTokenOptional | AuthScheme::AppserviceToken, Token::Appservice(info)) => {
			Ok(Auth {
				sender_user: None,
				sender_device: None,
				origin: None,
				appservice_info: Some(*info),
			})
		},
		(AuthScheme::AccessToken, Token::None) => {
			Err(Error::BadRequest(ErrorKind::MissingToken, "Missing access token."))
		},
		(
			AuthScheme::AccessToken | AuthScheme::AccessTokenOptional | AuthScheme::None,
			Token::User((user_id, device_id)),
		) => Ok(Auth {
			sender_user: Some(user_id),
			sender_device: Some(device_id),
			origin: None,
			appservice_info: None,
		}),
		(AuthScheme::ServerSignatures, Token::None) => Ok(auth_server(request).await?),
		(AuthScheme::None | AuthScheme::AppserviceToken | AuthScheme::AccessTokenOptional, Token::None) => Ok(Auth {
			sender_user: None,
			sender_device: None,
			origin: None,
			appservice_info: None,
		}),
		(AuthScheme::ServerSignatures, Token::Appservice(_) | Token::User(_)) => Err(Error::BadRequest(
			ErrorKind::Unauthorized,
			"Only server signatures should be used on this endpoint.",
		)),
		(AuthScheme::AppserviceToken, Token::User(_)) => Err(Error::BadRequest(
			ErrorKind::Unauthorized,
			"Only appservice access tokens should be used on this endpoint.",
		)),
	}
}

fn auth_appservice(request: &mut Request, info: Box<RegistrationInfo>) -> Result<Auth> {
	let user_id = request
		.query
		.user_id
		.clone()
		.map_or_else(
			|| {
				UserId::parse_with_server_name(
					info.registration.sender_localpart.as_str(),
					services().globals.server_name(),
				)
			},
			UserId::parse,
		)
		.map_err(|_| Error::BadRequest(ErrorKind::InvalidUsername, "Username is invalid."))?;

	if !info.is_user_match(&user_id) {
		return Err(Error::BadRequest(ErrorKind::Exclusive, "User is not in namespace."));
	}

	if !services().users.exists(&user_id)? {
		return Err(Error::BadRequest(ErrorKind::forbidden(), "User does not exist."));
	}

	Ok(Auth {
		sender_user: Some(user_id),
		sender_device: None,
		origin: None,
		appservice_info: Some(*info),
	})
}

async fn auth_server(request: &mut Request) -> Result<Auth> {
	if !services().globals.allow_federation() {
		return Err(Error::bad_config("Federation is disabled."));
	}

	let TypedHeader(Authorization(x_matrix)) = request
		.parts
		.extract::<TypedHeader<Authorization<XMatrix>>>()
		.await
		.map_err(|e| {
			warn!("Missing or invalid Authorization header: {e}");

			let msg = match e.reason() {
				TypedHeaderRejectionReason::Missing => "Missing Authorization header.",
				TypedHeaderRejectionReason::Error(_) => "Invalid X-Matrix signatures.",
				_ => "Unknown header-related error",
			};

			Error::BadRequest(ErrorKind::forbidden(), msg)
		})?;

	let origin_signatures = BTreeMap::from_iter([(x_matrix.key.clone(), CanonicalJsonValue::String(x_matrix.sig))]);

	let signatures = BTreeMap::from_iter([(
		x_matrix.origin.as_str().to_owned(),
		CanonicalJsonValue::Object(origin_signatures),
	)]);

	let server_destination = services().globals.server_name().as_str().to_owned();

	if let Some(destination) = x_matrix.destination.as_ref() {
		if destination != &server_destination {
			return Err(Error::BadRequest(ErrorKind::forbidden(), "Invalid authorization."));
		}
	}

	let signature_uri = CanonicalJsonValue::String(
		request
			.parts
			.uri
			.path_and_query()
			.unwrap_or(&PathAndQuery::from_static("/"))
			.to_string(),
	);

	let mut request_map = BTreeMap::from_iter([
		(
			"method".to_owned(),
			CanonicalJsonValue::String(request.parts.method.to_string()),
		),
		("uri".to_owned(), signature_uri),
		(
			"origin".to_owned(),
			CanonicalJsonValue::String(x_matrix.origin.as_str().to_owned()),
		),
		("destination".to_owned(), CanonicalJsonValue::String(server_destination)),
		("signatures".to_owned(), CanonicalJsonValue::Object(signatures)),
	]);

	if let Some(json_body) = &request.json {
		request_map.insert("content".to_owned(), json_body.clone());
	};

	let keys_result = services()
		.rooms
		.event_handler
		.fetch_signing_keys_for_server(&x_matrix.origin, vec![x_matrix.key.clone()])
		.await;

	let keys = keys_result.map_err(|e| {
		warn!("Failed to fetch signing keys: {e}");
		Error::BadRequest(ErrorKind::forbidden(), "Failed to fetch signing keys.")
	})?;

	let pub_key_map = BTreeMap::from_iter([(x_matrix.origin.as_str().to_owned(), keys)]);

	match ruma::signatures::verify_json(&pub_key_map, &request_map) {
		Ok(()) => Ok(Auth {
			sender_user: None,
			sender_device: None,
			origin: Some(x_matrix.origin),
			appservice_info: None,
		}),
		Err(e) => {
			warn!("Failed to verify json request from {}: {e}\n{request_map:?}", x_matrix.origin);

			if request.parts.uri.to_string().contains('@') {
				warn!(
					"Request uri contained '@' character. Make sure your reverse proxy gives Conduit the raw uri \
					 (apache: use nocanon)"
				);
			}

			Err(Error::BadRequest(
				ErrorKind::forbidden(),
				"Failed to verify X-Matrix signatures.",
			))
		},
	}
}
