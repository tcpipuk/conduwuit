use conduit::{err, Result};
use rocksdb::{Direction, ErrorKind, IteratorMode};

//#[cfg(debug_assertions)]
macro_rules! unhandled {
	($msg:literal) => {
		unimplemented!($msg)
	};
}

// activate when stable; we're not ready for this yet
#[cfg(disable)] // #[cfg(not(debug_assertions))]
macro_rules! unhandled {
	($msg:literal) => {
		// SAFETY: Eliminates branches for serializing and deserializing types never
		// encountered in the codebase. This can promote optimization and reduce
		// codegen. The developer must verify for every invoking callsite that the
		// unhandled type is in no way involved and could not possibly be encountered.
		unsafe {
			std::hint::unreachable_unchecked();
		}
	};
}

pub(crate) use unhandled;

#[inline]
pub(crate) fn _into_direction(mode: &IteratorMode<'_>) -> Direction {
	use Direction::{Forward, Reverse};
	use IteratorMode::{End, From, Start};

	match mode {
		Start | From(_, Forward) => Forward,
		End | From(_, Reverse) => Reverse,
	}
}

#[inline]
pub(crate) fn result<T>(r: std::result::Result<T, rocksdb::Error>) -> Result<T, conduit::Error> {
	r.map_or_else(or_else, and_then)
}

#[inline(always)]
pub(crate) fn and_then<T>(t: T) -> Result<T, conduit::Error> { Ok(t) }

pub(crate) fn or_else<T>(e: rocksdb::Error) -> Result<T, conduit::Error> { Err(map_err(e)) }

#[inline]
pub(crate) fn is_incomplete(e: &rocksdb::Error) -> bool { e.kind() == ErrorKind::Incomplete }

pub(crate) fn map_err(e: rocksdb::Error) -> conduit::Error {
	let string = e.into_string();
	err!(Database(error!("{string}")))
}
