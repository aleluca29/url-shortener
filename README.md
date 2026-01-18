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

### 10. Enhanced analytics (unique visitors + geo + daily)

The stats endpoint now returns more detailed analytics, including:

- unique_visitors
- clicks_by_day
- top_countries
- recent_clicks

```powershell
Invoke-RestMethod -Method GET `
  -Uri "http://localhost:3000/api/links/<CODE>/stats"
```

Expected: JSON includes `total_clicks` plus the fields above.

### 11. Rate limiting (10 requests/minute per IP)

```powershell
for ($i=1; $i -le 11; $i++) {
  try {
    Invoke-RestMethod -Method POST `
      -Uri "http://localhost:3000/api/shorten" `
      -ContentType "application/json" `
      -Body '{ "url": "https://example.com/rate" }'
    "OK"
  } catch {
    [int]$_.Exception.Response.StatusCode
  }
}
```
Expected: first ~10 requests return `OK`, then you get `429`

### 12. QR Code (PNG)

Create a short link (response includes `qr_png_url`):
```powershell
Invoke-RestMethod -Method POST `
  -Uri "http://localhost:3000/api/shorten" `
  -ContentType "application/json" `
  -Body '{ "url": "https://example.com", "custom_code": "qr1" }'
```

Get QR as PNG:

```powershell
curl.exe -I http://localhost:3000/api/links/qr1/qr
```

Expected: `200` OK and `Content-Type: image/png`

Download the QR image:

```powershell
curl.exe http://localhost:3000/api/links/qr1/qr -o qr1.png
```

Expected: a file named `qr1.png`

### 13. List all links

```powershell
Invoke-RestMethod -Method GET `
  -Uri "http://localhost:3000/api/links"
```

Expected: returns a list of all saved links with fields like `code`, `target_url` and click statistics.


### 14. Web dashboard (UI)

Open the dashboard in your browser:
- `http://localhost:3000/`

Expected:
- You can create short links from the UI
- You can see all saved links and their statistics

Open a link details page:
- `http://localhost:3000/links/<CODE>`

Expected:
- Shows total clicks, unique visitors, countries, recent clicks and QR code


## Database

SQLite file defaults to `dev.db` in the project root.
Migrations run automatically on startup.

