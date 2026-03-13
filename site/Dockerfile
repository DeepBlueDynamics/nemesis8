FROM nginx:alpine
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY index.html /usr/share/nginx/html/index.html
COPY nemesis-8.png /usr/share/nginx/html/nemesis-8.png
COPY octo.mp4 /usr/share/nginx/html/octo.mp4
EXPOSE 8080
CMD ["nginx", "-g", "daemon off;"]
