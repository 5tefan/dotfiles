autocmd BufRead,BufNewFile *.txt,*.md setlocal spell spelllang=en_us

set nocompatible
filetype plugin on
syntax on
set background=light
set nu
set laststatus=2
set tabstop=4
set softtabstop=0
set expandtab
set shiftwidth=4
set smarttab
set ruler
set list

" If you search for something containing uppercase characters, it will do a case sensitive search; 
" if you search for something purely lowercase, it will do a case insensitive search. 
" NOTE: ignorecase affects substitutions as well as searches.
set ignorecase
set smartcase

" keep 5 lines context on curosr... eg. when searching
set scrolloff=5


call plug#begin()

Plug 'hrsh7th/cmp-nvim-lsp'
Plug 'hrsh7th/nvim-cmp'
Plug 'hrsh7th/vim-vsnip'

Plug 'nvim-lua/plenary.nvim'
Plug 'scalameta/nvim-metals'
Plug 'neovim/nvim-lspconfig'
Plug 'p00f/clangd_extensions.nvim'

" Fuzzy finding
Plug 'junegunn/fzf'
Plug 'junegunn/fzf.vim'

" Surround text objects with quotes, etc.
Plug 'tpope/vim-surround'

" Repeat surrounds, etc.
Plug 'tpope/vim-repeat'

Plug 'lukas-reineke/indent-blankline.nvim'

" rust lsp
Plug 'simrat39/rust-tools.nvim'

call plug#end()


