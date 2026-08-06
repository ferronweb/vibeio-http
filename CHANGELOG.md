# `vibeio-http` change log

## `vibeio-http` UNRELEASED

**Not yet released**

- Added support for zerocopy file serving on FreeBSD.
- Reduced spurious errors when HTTP/1.x and HTTP/2 are abruptly closed while idle (accepting or upgraded)

## `vibeio-http` 0.3.4

**Released in June 26, 2026**

- Reduced spurious timeouts when kept-alive HTTP/1.x connection is idle

## `vibeio-http` 0.3.3

**Released in June 14, 2026**

- Improved `Expect: 100-continue` handling

## `vibeio-http` 0.3.2

**Released in June 6, 2026**

- Fixed chunk length parsing range in HTTP/1.x decoder
- Fixed DoS vulnerability in HTTP/1.x chunked encoding parser (triggered by maliciously crafted chunk lengths)
- Fixed off-by-one error in HTTP/1.x trailer header check
- The HTTP/1.x server logic now properly handles write-zero error when writing to the socket instead of infinitely looping

## `vibeio-http` 0.3.1

**Released in April 22, 2026**

- Improved graceful shutdown for HTTP/1.x

## `vibeio-http` 0.3.0

**Released in March 26, 2026**

- Performed some performance optimizations for HTTP/2

## `vibeio-http` 0.2.1

**Released in March 21, 2026**

- Improve HTTP/2 upgrade correctness

## `vibeio-http` 0.2.0

**Released in March 21, 2026**

- Added support for extended CONNECT for HTTP/2 and HTTP/3
- Performed some performance optimizations for HTTP/2

## `vibeio-http` 0.1.2

**Released in March 20, 2026**

- Added an option to disable sending `Date` header for HTTP/2 and HTTP/3
- The default maximum concurrent streams limit for HTTP/2 is now 200
- Fixed TE header being always removed for HTTP/2 and HTTP/3

## `vibeio-http` 0.1.1

**Released in March 19, 2026**

- Performed some performance optimizations for HTTP/1.x and HTTP/2

## `vibeio-http` 0.1.0

**Released in March 19, 2026**

- First release
