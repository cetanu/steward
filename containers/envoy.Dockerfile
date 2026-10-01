FROM envoyproxy/envoy:v1.39.0
ADD envoy.yaml /srv/envoy.yaml
CMD envoy -c /srv/envoy.yaml
