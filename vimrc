set nocompatible              " be iMproved, required
filetype off                  " required

" set the runtime path to include Vundle and initialize
set rtp+=~/.vim/bundle/Vundle.vim

" set runtime path to include fzf
set rtp+=/usr/local/opt/fzf

call vundle#begin()
" let Vundle manage Vundle, required
Plugin 'VundleVim/Vundle.vim'
Plugin 'tpope/vim-fugitive'
Plugin 'vimwiki/vimwiki'
Plugin 'scrooloose/nerdtree'
Plugin 'junegunn/fzf', { 'dir': '~/.fzf', 'do': './install --all' }
Plugin 'junegunn/fzf.vim'
Plugin 'lervag/vimtex'
" All of your Plugins must be added before the following line
call vundle#end()            " required
filetype plugin indent on    " required
" To ignore plugin indent changes, instead use:
"filetype plugin on
"
" Brief help
" :PluginList       - lists configured plugins
" :PluginInstall    - installs plugins; append `!` to update or just :PluginUpdate
" :PluginSearch foo - searches for foo; append `!` to refresh local cache
" :PluginClean      - confirms removal of unused plugins; append `!` to auto-approve removal
"
" see :h vundle for more details or wiki for FAQ
" Put your non-Plugin stuff after this line

" Turn filetype plugin back on...
" See :help ftplugin
filetype plugin on


syntax on
set nu
set laststatus=2
set tabstop=4 softtabstop=0 expandtab shiftwidth=4 smarttab
set ruler

" Plugin settings.

"" Vimwiki....
let wiki_1 = {}
let wiki_1.path = '~/ownCloud/vimwiki'
let wiki_1.path_html = '~/ownCloud/vimwiki_html/'

let wiki_2 = {}
let wiki_2.path = '~/ownCloud/vimwiki/work'
let wiki_2.path_html = '~/ownCloud/vimwiki_html/work/'

let g:vimwiki_list = [wiki_1, wiki_2]
autocmd FileType * setlocal formatoptions-=c formatoptions-=r formatoptions-=o

"" CtrlP
let g:ctrlp_map = '<c-p>'
let g:ctrlp_cmd = 'CtrlPMixed'


set backspace=indent,eol,start  " more powerful backspacing

set tw=100
set history=10000 " this is the max value

" If you search for something containing uppercase characters, it will do a case sensitive search; 
" if you search for something purely lowercase, it will do a case insensitive search. 
" NOTE: ignorecase affects substitutions as well as searches.
set ignorecase
set smartcase

" vimtex default flavor, rather than default plain
let g:tex_flavor = 'latex'

" highlight non-ascii characters: https://stackoverflow.com/a/16988346
syntax match nonascii "[^\x00-\x7F]"
highlight nonascii guibg=Red ctermbg=2

" when wrapping lines, move by displayed lines.
noremap <silent> k gk
noremap <silent> j gj

" keep 5 lines context on curosr... eg. when searching
set scrolloff=5
