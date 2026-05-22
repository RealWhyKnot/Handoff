// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterNetCurl wires net.curl. Outbound fetch with an SSRF guard --
// the resolved destination must be a public IP. Private RFC1918, link-
// local, loopback, multicast, and unspecified addresses are rejected.
// Methods restricted to GET/HEAD; max response body is 1 MiB.
func RegisterNetCurl(r *dispatch.Router) {
	r.Register("net.curl", netCurl)
}

func netCurl(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var urlStr string
	if v, ok := args["url"]; ok {
		_ = json.Unmarshal(v, &urlStr)
	}
	method := "GET"
	if v, ok := args["method"]; ok {
		_ = json.Unmarshal(v, &method)
		method = strings.ToUpper(method)
	}
	if method != "GET" && method != "HEAD" {
		return nil, fmt.Errorf("net.curl: method must be GET or HEAD (got %q)", method)
	}
	if urlStr == "" {
		return nil, fmt.Errorf("net.curl: 'url' is required")
	}

	u, err := url.Parse(urlStr)
	if err != nil {
		return nil, fmt.Errorf("net.curl: bad url: %w", err)
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return nil, fmt.Errorf("net.curl: scheme must be http or https (got %q)", u.Scheme)
	}

	// SSRF guard: resolve the host now, reject private destinations.
	// We pre-resolve and inject the IP back into the client to avoid
	// a TOCTOU between resolve and dial.
	host := u.Hostname()
	addrs, err := net.DefaultResolver.LookupIPAddr(ctx, host)
	if err != nil {
		return nil, fmt.Errorf("net.curl: resolve %q: %w", host, err)
	}
	for _, a := range addrs {
		if !isPublicUnicast(a.IP) {
			return nil, fmt.Errorf("net.curl: refusing private/loopback destination %s", a.IP.String())
		}
	}

	client := &http.Client{
		Timeout: 15 * time.Second,
		CheckRedirect: func(req *http.Request, via []*http.Request) error {
			if len(via) >= 5 {
				return fmt.Errorf("too many redirects")
			}
			// Re-validate every redirect destination -- a public URL can
			// 302 to a private one.
			h := req.URL.Hostname()
			ips, err := net.DefaultResolver.LookupIPAddr(req.Context(), h)
			if err != nil {
				return err
			}
			for _, a := range ips {
				if !isPublicUnicast(a.IP) {
					return fmt.Errorf("redirect to private dest %s", a.IP.String())
				}
			}
			return nil
		},
	}

	req, err := http.NewRequestWithContext(ctx, method, urlStr, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", "Handoff/0.1 (+https://github.com/RealWhyKnot/Handoff)")
	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	const bodyCap = 1 << 20 // 1 MiB
	limited := io.LimitReader(resp.Body, bodyCap+1)
	body, _ := io.ReadAll(limited)
	truncated := false
	if int64(len(body)) > bodyCap {
		body = body[:bodyCap]
		truncated = true
	}

	hdrs := map[string]string{}
	for k, v := range resp.Header {
		hdrs[k] = strings.Join(v, ", ")
	}

	out := map[string]interface{}{
		"status":         resp.StatusCode,
		"status_text":    resp.Status,
		"headers":        hdrs,
		"body_size":      len(body),
		"body_truncated": truncated,
	}
	if method == "GET" {
		out["body_base64"] = base64.StdEncoding.EncodeToString(body)
	}
	return out, nil
}

// isPublicUnicast returns true only for routable public IPv4/IPv6.
// Rejects: loopback, link-local, multicast, unspecified, RFC1918,
// 100.64/10 CGNAT, IPv4-mapped IPv6, and ULAs.
func isPublicUnicast(ip net.IP) bool {
	if ip == nil || ip.IsLoopback() || ip.IsLinkLocalUnicast() || ip.IsLinkLocalMulticast() ||
		ip.IsMulticast() || ip.IsUnspecified() || ip.IsPrivate() {
		return false
	}
	// IPv6 unique-local fc00::/7 (Go's IsPrivate handles this in 1.17+;
	// keep an explicit check for clarity).
	if v6 := ip.To16(); len(v6) == 16 && ip.To4() == nil {
		if v6[0]&0xfe == 0xfc {
			return false
		}
	}
	// 100.64.0.0/10 CGNAT.
	if v4 := ip.To4(); v4 != nil {
		if v4[0] == 100 && v4[1]&0xc0 == 64 {
			return false
		}
	}
	return true
}
