# gettext 工作流(Rust 源码,标准 GNU 流程):
#   make pot        源码变更后重新提取 po/gdebi.pot(xgettext)
#   make merge-po   将 pot 合并回各 *.po,保留既有翻译(未精确匹配的标记 fuzzy)
#   make mo         编译 *.po → locale/<lang>/LC_MESSAGES/gdebi.mo(msgfmt --check)
#   make install    安装 .mo 到 $(DESTDIR)/$(LOCALEDIR)/<lang>/LC_MESSAGES/
#   make check      校验各 .po 的格式与一致性
#   make clean      清理编译产物(locale/)
#
# 源码内 msgid 经 gettext()/ngettext()/tf() 标注,提取关键字见 pot 目标。
# 本地运行(未安装):
#   make -C po mo
#   GDEBI_LOCALEDIR=$PWD/po/locale LC_ALL=zh_CN.utf8 cargo run -p gdebi-gtk

DOMAIN   ?= gdebi
VERSION  := $(shell sed -n 's/^version *= *"\(.*\)"/\1/p' ../Cargo.toml | head -1)
LOCALEDIR ?= /usr/share/locale
DESTDIR   ?=

# POTFILES.in 相对本目录,Makefile 内部以 ../ 前缀引用仓库根
POTFILES := $(shell sed -e 's/^#.*//' -e '/^$$/d' -e '/^\[/d' POTFILES.in | sed 's|^|../|')
POFILES  := $(filter-out $(DOMAIN).pot,$(wildcard *.po))
LANGS    := $(POFILES:.po=)
MOFILES  := $(LANGS:%=locale/%/LC_MESSAGES/$(DOMAIN).mo)

XGETTEXT ?= xgettext
MSGMERGE ?= msgmerge
MSGFMT   ?= msgfmt

XGETTEXT_ARGS := --language=Rust --from-code=UTF-8 --add-comments=TRANSLATORS \
  --keyword=gettext --keyword=ngettext:1,2 --keyword=tf:1 \
  --package-name=$(DOMAIN) --package-version=$(VERSION) \
  --msgid-bugs-address=sebastian.heinlein@web.de

.PHONY: all pot merge-po mo install check clean

all: mo

$(DOMAIN).pot: $(POTFILES) POTFILES.in
	$(XGETTEXT) $(XGETTEXT_ARGS) -o $@ $(POTFILES)

pot: $(DOMAIN).pot

merge-po: $(DOMAIN).pot
	@for f in $(POFILES); do \
	  echo "  MSGMERGE $$f"; \
	  $(MSGMERGE) --update --backup=none --previous $$f $(DOMAIN).pot; \
	done

mo: $(MOFILES)

locale/%/LC_MESSAGES/$(DOMAIN).mo: %.po
	@mkdir -p $(dir $@)
	$(MSGFMT) --check -o $@ $<

check: $(POFILES)
	@for f in $^; do $(MSGFMT) --check -o /dev/null $$f || exit 1; done

install: mo
	@for lang in $(LANGS); do \
	  install -Dm644 locale/$$lang/LC_MESSAGES/$(DOMAIN).mo \
	    $(DESTDIR)$(LOCALEDIR)/$$lang/LC_MESSAGES/$(DOMAIN).mo; \
	  echo "  INSTALL $$lang"; \
	done

clean:
	rm -rf locale
