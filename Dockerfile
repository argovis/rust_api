FROM rust:1.81.0

RUN apt-get update -y && apt-get install -y nano curl wget libhdf5-serial-dev libnetcdff-dev netcdf-bin
WORKDIR /app
COPY . .
#RUN chown -R 1000660000 /app
#CMD bash run.sh
