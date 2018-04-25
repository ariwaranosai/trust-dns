#!/bin/bash -e

set -x

trust_dns_dir=$(dirname $0)/..
cd ${trust_dns_dir:?}

case $(uname) in
  Darwin) exit 0;;
  *)      KCOV=true;;
esac

# don't run on nightly or beta
rustc --version | grep beta && exit 0;
rustc --version | grep nightly && exit 0;
if [ -z ${RUN_KCOV} ] ; then exit 0; fi

rm -rf kcov-master master.tar.gz*

# install kcov
# sudo apt-get install libcurl4-openssl-dev libelf-dev libdw-dev
sudo apt-get install cmake libcurl4-openssl-dev libelf-dev libdw-dev
wget https://github.com/SimonKagstrom/kcov/archive/master.tar.gz
tar xzf master.tar.gz
mkdir kcov-master/build
cd kcov-master/build
cmake ..
make
sudo make install
cd ../..

# run kcov on all tests, rerunning all tests with coverage report
mkdir -p target

# needed to tell some config tests where the server root directory is
export TDNS_SERVER_SRC_ROOT=./server
export COVERALLS_PARALLEL=true

SRC_PATHS=client/src,native-tls/src,openssl/src,proto/src,resolver/src,rustls/src,server/src
EXCLUDE_PATHS=client/src/error,proto/src/error.rs,server/src/error,compatibility-tests/src/lib.rs

# Build all tests
cargo build --tests --manifest-path proto/Cargo.toml --all-features  
cargo build --tests --manifest-path client/Cargo.toml --all-features
cargo build --tests --manifest-path native-tls/Cargo.toml --all-features
cargo build --tests --manifest-path openssl/Cargo.toml --all-features
cargo build --tests --manifest-path rustls/Cargo.toml --all-features
cargo build --tests --manifest-path resolver/Cargo.toml --all-features
cargo build --tests --manifest-path server/Cargo.toml --all-features
cargo build --tests --manifest-path integration-tests/Cargo.toml --all-features

for i in target/debug/deps/trust_dns*-* target/debug/deps/*_tests-* ; do
  if [ -f $i ] && [ -x $i ]; then
    # submit the report... what's the executable since there are many?
    echo "----> executing kcov on $i"
    kcov --exclude-pattern=/.cargo \
         --include-path=${SRC_PATHS} \
         --exclude-path=${EXCLUDE_PATHS} \
         target/kcov-$(basename $i) $i

    let test_count='test_count+1'
    
    # this only works for a single test run upload
    last_test=$i
  fi
done

echo "----> ran $test_count test(s)"

echo "----> uploading to codecov.io"
bash <(curl -s https://codecov.io/bash)
echo "----> coverage reports done"