# Design Decisions

## Technology choices
- **Rust + Axum:** chosen for performance, safety, and async support.
- **SQLite:** chosen for simplicity and easy local development.

## Short code generation
- Random 6–8 character alphanumeric codes.
- Collision handled by retrying generation until insert succeeds.

## Analytics approach
- Each redirect is stored as a click event.
- Statistics are computed from stored events.

## Rate limiting
- Fixed window rate limiting (10 requests/minute per IP).
- Implemented in-memory.

## Expiration
- Links are checked for expiration at redirect time.
- Expired links return HTTP 410.
