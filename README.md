# URL Shortener with Analytics (Rust + Axum + SQLite)

This repo starts from a Week 7 milestone (basic shorten + redirect + click counting) and evolves into a full project:

- REST API for shortening, redirecting, and analytics
- Short code generation with collision handling
- Optional custom short codes
- Rate limiting (10 requests/minute per IP)
- Expiration dates
- QR code generation
- Web dashboard

> Note: Some features from the list above will be added in future commits.  
> Current progress includes shortening, redirecting, click counting, custom codes and expiration validation.

## Run (dev)

```bash
cargo run
```

Then open:
- Health: `http://localhost:3000/health`

## API Testing

### 1. Health Check
```powershell
curl -UseBasicParsing http://localhost:3000/health
```
Expected: `ok`

### 2. Create a Short Link (random code)
```powershell
Invoke-RestMethod -Method POST `
  -Uri "http://localhost:3000/api/shorten" `
  -ContentType "application/json" `
  -Body '{ "url": "https://www.rust-lang.org/learn" }'
```
Expected: returns `code` + `short_url`

### 3. Redirect
```powershell
curl.exe -i http://localhost:3000/<CODE>
```
Expected: `307 Temporary Redirect` and a `Location:` header

### 4. Stats (total clicks)
```powershell
curl.exe http://localhost:3000/api/links/<CODE>/stats
```
Expected:
```json
{"total_clicks":1}
```

### 5. Create a Short Link (custom code)
```powershell
Invoke-RestMethod -Method POST `
  -Uri "http://localhost:3000/api/shorten" `
  -ContentType "application/json" `
  -Body '{ "url": "https://docs.rs/sqlx/latest/sqlx/", "custom_code": "sqlxdocs" }'
```
Expected: returns `code = sqlxdocs`

### 6. Custom code already exists (409)
```powershell
try {
  Invoke-RestMethod -Method POST `
    -Uri "http://localhost:3000/api/shorten" `
    -ContentType "application/json" `
    -Body '{ "url": "https://example.org/another-page", "custom_code": "sqlxdocs" }'
} catch {
  [int]$_.Exception.Response.StatusCode
}
```
Expected: `409`

### 7. Invalid expiration format (400)
```powershell
try {
  Invoke-RestMethod -Method POST `
    -Uri "http://localhost:3000/api/shorten" `
    -ContentType "application/json" `
    -Body '{ "url": "https://www.mozilla.org", "expires_at": "tomorrow" }'
} catch {
  [int]$_.Exception.Response.StatusCode
}
```
Expected: `400`

### 8. Expired link redirect (410)
```powershell
Invoke-RestMethod -Method POST `
  -Uri "http://localhost:3000/api/shorten" `
  -ContentType "application/json" `
  -Body '{ "url": "https://example.com", "custom_code": "expired1", "expires_at": "2000-01-01T00:00:00Z" }'

curl.exe -i http://localhost:3000/expired1
```
Expected: `410 Gone`

### 9. Redirect with metadata headers
```powershell
Invoke-RestMethod -Method POST `
  -Uri "http://localhost:3000/api/shorten" `
  -ContentType "application/json" `
  -Body '{ "url": "https://www.rust-lang.org", "custom_code": "meta1" }'

curl.exe -i http://localhost:3000/meta1 `
  -H "X-Forwarded-For: 1.2.3.4" `
  -H "User-Agent: test-agent" `
  -H "Referer: https://ref.example" `
  -H "CF-IPCountry: RO"
```
Expected: `307 Temporary Redirect`

## Database

SQLite file defaults to `dev.db` in the project root.
Migrations run automatically on startup.

