# web-jingzi

mirror of website with domain name substitution

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
