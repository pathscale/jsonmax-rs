use jmax_abi::span::Span32;
use jmax_http::*;
use std::sync::Arc;

fn headers() -> Result<Vec<Header>> {
    Ok(vec![
        Header::new("content-type", "application/json")?,
        Header::new("content-length", "4")?,
    ])
}

#[test]
fn arc_body_subview_moves_into_request_job_without_copying() -> Result<()> {
    let owner = Arc::new(b"header:null:pipeline".to_vec());
    let weak = Arc::downgrade(&owner);
    let pointer = owner.as_ptr().wrapping_add(7);
    let body = body::receive_arc_subview(owner, Span32::new(7, 4))?;
    assert_eq!(body.as_slice().as_ptr(), pointer);
    assert_eq!(body.backing_len_lower_bound(), 20);
    let request = IncomingRequest {
        method: HttpMethod::Post,
        uri: "/".into(),
        headers: headers()?,
        body,
        completion: BodyCompletion::Complete,
    };
    let job = body::request_job(
        request,
        HttpLimits::default(),
        None,
        CancellationToken::new(),
    )?;
    assert_eq!(job.payload.as_slice(), b"null");
    assert_eq!(job.payload.as_slice().as_ptr(), pointer);
    assert!(weak.upgrade().is_some());
    drop(job);
    assert!(weak.upgrade().is_none());
    Ok(())
}

#[test]
fn bytes_body_subview_moves_into_response_job_and_checks_completion() -> Result<()> {
    let bytes = bytes::Bytes::from_static(b"prefixnullsuffix");
    let pointer = bytes.as_ptr().wrapping_add(6);
    let body = body::receive_bytes_subview(bytes, Span32::new(6, 4))?;
    assert_eq!(body.as_slice().as_ptr(), pointer);
    let response = IncomingResponse {
        status: 200,
        headers: headers()?,
        body,
        completion: BodyCompletion::Complete,
    };
    let job = body::response_job(
        response.clone(),
        HttpLimits::default(),
        CancellationToken::new(),
    )?;
    assert_eq!(job.payload.as_slice().as_ptr(), pointer);
    let incomplete = IncomingResponse {
        completion: BodyCompletion::Incomplete,
        ..response
    };
    assert!(
        matches!(body::response_job(incomplete, HttpLimits::default(), CancellationToken::new()), Err(e) if e.code == ErrorCode::TransportIncomplete)
    );
    Ok(())
}

#[test]
fn nested_arc_slice_subviews_keep_owner_and_reject_invalid_ranges() -> Result<()> {
    let owner: Arc<[u8]> = Arc::from(&b"xxnullxx"[..]);
    let payload = Payload::Shared(owner);
    let view = body::subview(&payload, Span32::new(1, 6))?;
    let nested = body::subview(&view, Span32::new(1, 4))?;
    assert_eq!(nested.as_slice(), b"null");
    assert_eq!(
        nested.as_slice().as_ptr(),
        payload.as_slice().as_ptr().wrapping_add(2)
    );
    assert!(nested.shares_backing_with(&payload));
    assert!(body::subview(&nested, Span32::new(3, 2)).is_err());
    assert!(body::subview(&nested, Span32::new(u32::MAX, 4)).is_err());
    Ok(())
}
