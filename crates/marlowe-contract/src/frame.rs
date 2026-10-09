//! CONTRACTS.md section 4.0.3 — the frame, and section 4.0.4 — the error taxonomy.
//!
//! Framing and nothing else. It adds no field that carries meaning and it is not a fourth
//! interface (section 4.0). Section 4.0.8: for a given section 4 body the frame bytes are a
//! pure function of that body.

use serde::{Deserialize, Serialize};

/// `op` ∈ ingest | retrieve | answer, selecting sections 4.6, 4.1–4.4, and 4.7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Ingest,
    Retrieve,
    Answer,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Ingest => "ingest",
            Op::Retrieve => "retrieve",
            Op::Answer => "answer",
        }
    }
}

/// A request frame. `body` stays as raw JSON until the op is known — the op selects which
/// section 4 type to parse it as, and parsing it twice would be the only alternative.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestFrame {
    pub op: Op,
    pub body: serde_json::Value,
}

/// Section 4.0.4. **`error` is the absence of a section 4 response, not a variant of one.**
///
/// Two classes, handled differently because they mean different things:
///
/// - **Class A** — the request was bad. The harness or the version pairing is at fault, and
///   the harness aborts the run. A defect, not a measurement.
/// - **Class B** — `internal_error`. The system under test failed on a well-formed request.
///   The harness records a failed unit and continues. **A result**, and it appears in the
///   report.
///
/// Note what does *not* belong here: section 4.6's `rejected` is a **successful** response —
/// the implementation understood the request and refused a write — and it travels in `body`.
/// The poisoning suite asserts on exactly that, so emitting a refusal as an `error` would
/// make a working defence look like a failure and zero out K3 for the wrong reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    // class A
    MalformedFrame,
    UnknownOp,
    MalformedBody,
    ContractVersionUnsupported,
    // class B
    InternalError,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameError {
    pub kind: ErrorKind,
    pub detail: String,
}

/// A response frame. Exactly one of `body` and `error` is present — enforced by the type,
/// because a struct with two Options would let both or neither be set.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ResponseFrame {
    Body {
        op: &'static str,
        body: serde_json::Value,
    },
    Error {
        op: &'static str,
        error: FrameError,
    },
}

impl ResponseFrame {
    pub fn body(op: Op, body: serde_json::Value) -> Self {
        ResponseFrame::Body {
            op: op.as_str(),
            body,
        }
    }

    pub fn error(op: Op, kind: ErrorKind, detail: impl Into<String>) -> Self {
        ResponseFrame::Error {
            op: op.as_str(),
            error: FrameError {
                kind,
                detail: detail.into(),
            },
        }
    }

    /// Serialize to one line of NDJSON, with no terminator.
    ///
    /// Section 4.0.2: a frame contains no literal newline, and pretty-printed output is a
    /// protocol error rather than a tolerated variation. `to_string` (not `to_string_pretty`)
    /// is what holds that, and the assertion below is what keeps it held.
    pub fn to_line(&self) -> String {
        let line = serde_json::to_string(self).expect("response frame must serialize");
        debug_assert!(
            !line.contains('\n'),
            "a frame must not contain a literal newline (section 4.0.2)"
        );
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_and_error_frames_are_mutually_exclusive_by_construction() {
        let ok = ResponseFrame::body(Op::Ingest, serde_json::json!({"session_id": "s"}));
        let line = ok.to_line();
        assert!(line.contains("\"op\":\"ingest\""));
        assert!(line.contains("\"body\""));
        assert!(!line.contains("\"error\""));

        let bad = ResponseFrame::error(Op::Retrieve, ErrorKind::InternalError, "index not loaded");
        let line = bad.to_line();
        assert!(line.contains("\"error\""));
        assert!(!line.contains("\"body\""));
        assert!(line.contains("\"internal_error\""));
    }

    #[test]
    fn frames_are_single_line() {
        let frame = ResponseFrame::body(
            Op::Answer,
            serde_json::json!({"answer": "line one\nline two"}),
        );
        let line = frame.to_line();
        assert!(
            !line.contains('\n'),
            "an embedded newline must be escaped, not emitted raw"
        );
        assert!(line.contains("\\n"), "it should appear escaped");
    }

    #[test]
    fn unknown_op_fails_to_parse() {
        let err = serde_json::from_str::<RequestFrame>(r#"{"op":"reticulate","body":{}}"#);
        assert!(err.is_err());
    }

    #[test]
    fn extra_frame_level_key_is_rejected() {
        // Section 4.0.3: any additional key at frame level is a protocol error.
        let err = serde_json::from_str::<RequestFrame>(r#"{"op":"ingest","body":{},"trace":"x"}"#);
        assert!(err.is_err());
    }
}
