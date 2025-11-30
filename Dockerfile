FROM debian:13

RUN DEBIAN_FRONTEND=noninteractive apt-get update && apt-get install -y \
    build-essential \
    make \
    cmake \
    pkg-config \
    git \
    curl \
    wget \
    ca-certificates \
    fish \
    sudo \
    locales

RUN curl -LO https://github.com/neovim/neovim/releases/latest/download/nvim-linux-x86_64.appimage && \
    chmod +x nvim-linux-x86_64.appimage && \
    ./nvim-linux-x86_64.appimage --appimage-extract && \
    mv squashfs-root /opt/nvim && \
    ln -s /opt/nvim/usr/bin/nvim /usr/local/bin/nvim && \
    ln -s /opt/nvim/usr/bin/nvim /usr/local/bin/vim && \
    rm nvim-linux-x86_64.appimage

# Install Node.js (required for Claude Code)
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y nodejs && \
    rm -rf /var/lib/apt/lists/*
RUN npm install -g @anthropic-ai/claude-code

ARG USER_NAME=developer
ARG USER_ID=1000
ARG GROUP_ID=1000

RUN groupadd -g ${GROUP_ID} ${USER_NAME} || groupadd ${USER_NAME} && \
    useradd -m -u ${USER_ID} -g ${USER_NAME} -s /usr/bin/fish ${USER_NAME} && \
    echo "${USER_NAME} ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

# Switch to the created user
USER ${USER_NAME}
WORKDIR /home/${USER_NAME}

# Install Rust via rustup for the user
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

# Add cargo to PATH
ENV PATH="/home/${USER_NAME}/.cargo/bin:${PATH}"
