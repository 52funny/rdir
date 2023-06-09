use std::convert::Infallible;
use warp::{Rejection, Reply};

#[derive(Debug)]
pub enum ReplyError {
    InternalError,
    BytesRangeError,
}

impl warp::reject::Reject for ReplyError {}

pub async fn error_recover(err: Rejection) -> Result<impl Reply, Infallible> {
    if err.is_not_found() {
        return Ok(warp::reply::with_status(
            "Not Found",
            warp::http::StatusCode::NOT_FOUND,
        ));
    }
    match err.find() {
        Some(&ReplyError::InternalError) => Ok(warp::reply::with_status(
            "Internal Server Error",
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
        Some(&ReplyError::BytesRangeError) => Ok(warp::reply::with_status(
            "Range Not Satisfiable",
            warp::http::StatusCode::RANGE_NOT_SATISFIABLE,
        )),
        _ => Ok(warp::reply::with_status(
            "Internal Server Error",
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}
