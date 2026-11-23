# Babelfish for PostgreSQL (SDB-313): SQL Server's protocol (TDS) in front of PostgreSQL, built
# from its open-source repositories, since it has no official image. The two repositories are
# pinned by commit (the tags BABEL_5_6_0 and BABEL_5_6_0__PG_17_10), and ANTLR's C++ runtime
# by version and checksum. The build takes a long while (tens of minutes); the image is built
# once and kept.
FROM ubuntu:22.04@sha256:f64b2ffc2f10ab8f8505aa56b2664bd846cbb9df87c7e2532e30f7667021c0e6 AS build

ARG PG_COMMIT=c33d595c9470394483ea345563ded89b681ac0e3
ARG EXTENSIONS_COMMIT=66fe10002e374eaae8e4749fa2ee7c69d321f572
ARG ANTLR=4.13.2
ARG ANTLR_RUNTIME_SHA256=0ed13668906e86dbc0dcddf30fdee68c10203dea4e83852b4edb810821bee3c4

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential bison flex gawk pkg-config cmake git ca-certificates curl unzip \
        openjdk-21-jre-headless uuid-dev libossp-uuid-dev libicu-dev libxml2-dev libxml2-utils \
        libxslt1-dev xsltproc libssl-dev openssl python3-dev libpq-dev libutfcpp-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
# Each repository at its pinned commit, nothing else of its history.
RUN git init -q postgresql && cd postgresql \
    && git fetch -q --depth 1 https://github.com/babelfish-for-postgresql/postgresql_modified_for_babelfish.git "$PG_COMMIT" \
    && git checkout -q FETCH_HEAD
RUN git init -q extensions && cd extensions \
    && git fetch -q --depth 1 https://github.com/babelfish-for-postgresql/babelfish_extensions.git "$EXTENSIONS_COMMIT" \
    && git checkout -q FETCH_HEAD

# The patched PostgreSQL, with TLS so the TDS login can be encrypted.
RUN cd postgresql \
    && ./configure --prefix=/opt/babelfish --without-readline --with-libxml --with-uuid=ossp \
        --with-icu --with-openssl \
    && make -j"$(nproc)" && make install \
    && cd contrib && make -j"$(nproc)" && make install

# ANTLR's C++ runtime, which babelfishpg_tsql's parser needs; the jar comes with the extensions.
RUN cp extensions/contrib/babelfishpg_tsql/antlr/thirdparty/antlr/antlr-${ANTLR}-complete.jar /usr/local/lib/ \
    && curl -fsSL -o antlr.zip "https://www.antlr.org/download/antlr4-cpp-runtime-${ANTLR}-source.zip" \
    && echo "${ANTLR_RUNTIME_SHA256}  antlr.zip" | sha256sum -c - \
    && unzip -q -d antlr4 antlr.zip && mkdir antlr4/build && cd antlr4/build \
    && cmake .. -DANTLR_JAR_LOCATION=/usr/local/lib/antlr-${ANTLR}-complete.jar \
        -DCMAKE_INSTALL_PREFIX=/usr/local -DWITH_DEMO=False -Wno-dev \
    && make -j"$(nproc)" && make install \
    && cp /usr/local/lib/libantlr4-runtime.so.${ANTLR} /opt/babelfish/lib/

# The four extensions. babelfishpg_tsql's sources include the parser ANTLR generates in the same
# make: in parallel, they are compiled before it exists, so that one builds its parser first.
ENV PG_CONFIG=/opt/babelfish/bin/pg_config PG_SRC=/src/postgresql cmake=/usr/bin/cmake
RUN cd extensions/contrib \
    && for ext in babelfishpg_money babelfishpg_common babelfishpg_tds; do \
        (cd "$ext" && make -j"$(nproc)" && make install) || exit 1; \
    done \
    && cd babelfishpg_tsql && make antlr/libantlr_tsql.a && make -j"$(nproc)" && make install

FROM ubuntu:22.04@sha256:f64b2ffc2f10ab8f8505aa56b2664bd846cbb9df87c7e2532e30f7667021c0e6
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        libossp-uuid16 libicu70 libxml2 libxslt1.1 libssl3 openssl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -s /bin/bash postgres
COPY --from=build /opt/babelfish /opt/babelfish
COPY --from=build /usr/local/lib/libantlr4-runtime.so* /usr/local/lib/
RUN ldconfig
COPY start.sh /usr/local/bin/start-babelfish
RUN chmod 755 /usr/local/bin/start-babelfish
USER postgres
ENV PATH=/opt/babelfish/bin:$PATH PGDATA=/home/postgres/data
EXPOSE 1433 5432
CMD ["start-babelfish"]
