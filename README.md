# web-jingzi

mirror of website with domain name substitution

## config file:

```yaml
listen_address: 127.0.0.1:3003
# optional, if set, will forward all connect to this proxy
socks5_server: 127.0.0.1:1080
domain_name:
  x.com: www.google.com
  y.com: wikipedia.org
```

with nginx:

```nginx
    server {
        server_name x.com;

        location / { 
            proxy_http_version 1.1;
            proxy_set_header Host $http_host;
            proxy_pass http://127.0.0.1:3003;
        }
    }
```
