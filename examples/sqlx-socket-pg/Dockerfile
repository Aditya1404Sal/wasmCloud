FROM postgres:17-alpine

COPY certs/server.crt /var/lib/postgresql/server.crt
COPY certs/ca.crt /var/lib/postgresql/ca.crt
COPY certs/keys/server.key /var/lib/postgresql/server.key

RUN chown 70:70 /var/lib/postgresql/server.crt /var/lib/postgresql/ca.crt /var/lib/postgresql/server.key
RUN chmod 0600 /var/lib/postgresql/server.key
