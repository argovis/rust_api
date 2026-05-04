FROM rust:1.88

RUN apt-get update -y && apt-get install -y nano curl wget libhdf5-serial-dev libnetcdff-dev netcdf-bin

COPY api /app
WORKDIR /app
RUN cargo build --release
CMD ["/app/target/release/api"]

#RUN chown -R 1000660000 /app
#CMD bash run.sh
