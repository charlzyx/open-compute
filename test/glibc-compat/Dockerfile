FROM quay.io/pypa/manylinux_2_28_x86_64@sha256:1e7b8460ad8fb3fb0fbe1cd818845159ac896b4fd17452d56e892c291f3b97a8

ENV PATH=/opt/python/cp313-cp313/bin:/opt/llvm/bin:$PATH
RUN dnf install -y git tar gzip xz unzip zip patch libxml2 libicu libzstd zlib-devel which \
    && dnf clean all \
    && python -m pip install --no-cache-dir cmake==3.31.6 ninja==1.13.0 pyelftools==0.33

RUN mkdir -p /opt/llvm /build \
    && curl -fL https://mirrors.edge.kernel.org/pub/tools/llvm/files/llvm-22.1.8-x86_64.tar.xz -o /build/clang.tar.xz \
    && echo '8d5d1e52a06e1c10e457fd36489047053ac451ddad9c88017b0d0fc6b8e5d4c0  /build/clang.tar.xz' | sha256sum -c - \
    && tar -xJf /build/clang.tar.xz --strip-components=1 -C /opt/llvm \
    && clang --version \
    && curl -fL https://github.com/llvm/llvm-project/releases/download/llvmorg-22.1.8/llvm-project-22.1.8.src.tar.xz -o /build/llvm.tar.xz \
    && echo '922f1817a0df7b1489272d18134ee0087a8b068828f87ac63b9861b1a9965888  /build/llvm.tar.xz' | sha256sum -c - \
    && tar -xJf /build/llvm.tar.xz -C /build \
    && cmake -G Ninja -S /build/llvm-project-22.1.8.src/runtimes -B /build/runtimes \
         -DCMAKE_BUILD_TYPE=Release -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ \
         -DCMAKE_INSTALL_PREFIX=/opt/llvm -DCMAKE_INSTALL_LIBDIR=lib \
         -DLLVM_ENABLE_PER_TARGET_RUNTIME_DIR=OFF \
         '-DLLVM_ENABLE_RUNTIMES=libcxx;libcxxabi;libunwind' \
         -DLIBCXX_ENABLE_SHARED=OFF -DLIBCXXABI_ENABLE_SHARED=OFF -DLIBUNWIND_ENABLE_SHARED=OFF \
         -DLIBCXXABI_USE_LLVM_UNWINDER=ON \
         -DLIBCXX_STATICALLY_LINK_ABI_IN_STATIC_LIBRARY=ON \
         -DLIBCXXABI_STATICALLY_LINK_UNWINDER_IN_STATIC_LIBRARY=ON \
    && cmake --build /build/runtimes --parallel 4 \
    && cmake --install /build/runtimes \
    && rm -rf /build

RUN curl -fL https://github.com/bazelbuild/bazel/releases/download/9.2.0/bazel-9.2.0-linux-x86_64 -o /usr/local/bin/bazel \
    && echo '7668a95db1250f12c40407251e4e203b4ec8bf39bc495d2f485b2d8c99048694  /usr/local/bin/bazel' | sha256sum -c - \
    && chmod 0755 /usr/local/bin/bazel \
    && bazel --version
