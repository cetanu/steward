import os
import shutil
import zipfile
import requests
import structlog
from collections import namedtuple
from pathlib import Path

structlog.configure()
logger = structlog.get_logger()

build_directory = Path("proto")
if not build_directory.exists():
    logger.msg("Creating proto directory")
    build_directory.mkdir()
os.chdir(build_directory)

utf8 = "utf-8"

ENVOY_VERSION = "1.25.0"

Package = namedtuple("Package", ["url", "name", "directory", "namespace"])
packages = {
    Package(
        url=f"https://github.com/envoyproxy/envoy/archive/refs/tags/v{ENVOY_VERSION}.zip",
        name="envoy",
        namespace="envoy",
        directory=f"envoy-{ENVOY_VERSION}/api",
    ),
    Package(
        url="https://github.com/googleapis/googleapis/archive/master.zip",
        name="google",
        namespace="google",
        directory="googleapis-master",
    ),
    Package(
        url="https://github.com/cncf/udpa/archive/refs/tags/v0.0.1.zip",
        name="udpa",
        namespace="udpa",
        directory="udpa-0.0.1",
    ),
    Package(
        url="https://github.com/envoyproxy/protoc-gen-validate/archive/main.zip",
        name="validate",
        namespace="validate",
        directory="protoc-gen-validate-main",
    ),
    Package(
        url="https://github.com/census-instrumentation/opencensus-proto/archive/refs/tags/v0.2.0.zip",
        name="opencensus",
        namespace="opencensus",
        directory="opencensus-proto-0.2.0/src",
    ),
    Package(
        url="https://github.com/prometheus/client_model/archive/refs/tags/v0.2.0.zip",
        name="prometheus",
        namespace=".",
        directory="client_model-0.2.0",
    ),
    Package(
        url="https://github.com/cncf/xds/archive/refs/heads/main.zip",
        name="xds",
        namespace="xds",
        directory="xds-main",
    ),
}


for package in packages:
    name = Path(package.name)
    namespace = Path(package.namespace)
    proto_root = Path(package.directory)
    zipname = f"{package.name}.zip"
    archive = Path(zipname)

    if not archive.exists():
        logger.msg(f"Downloading {package.name} protocol buffers from Github")
        with open(archive, "wb+") as zf:
            zf.write(requests.get(package.url).content)

    if name.exists():
        logger.msg(f"{name.absolute()} exists")
    else:
        logger.msg(f"Extracting {name} archive")
        with zipfile.ZipFile(archive, "r") as zipref:
            protos = [
                member for member in zipref.namelist() if member.endswith(".proto")
            ]
            for file in protos:
                logger.msg(f"Extracting {file}")
                zipref.extract(member=file, path=".")
        shutil.copytree(proto_root / namespace, name)

        logger.msg("Cleaning up old directory and zipfile")
        shutil.rmtree(proto_root.parts[0])
        os.remove(zipname)

